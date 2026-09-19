//! Minimal ARSY command line: `arsy run`, `arsy resume`, and `arsy doctor`.
//!
//! The surface, output modes, and exit codes follow `docs/36-cli-tui.md`;
//! diagnostic codes follow `docs/33-diagnostics.md`. Codes owned here:
//!
//! | Code | Condition |
//! |---|---|
//! | `ARSY-SCH-1000` | unknown command |
//! | `ARSY-SCH-1001` | usage error: unknown flag, missing or extra argument, bad value |
//! | `ARSY-SCH-1002` | documented command that its roadmap phase has not shipped yet |
//! | `ARSY-SCH-1003` | bare `arsy`: no terminal, or TUI disabled at build time |
//! | `ARSY-SCH-1004` | `resume` named a session with no recorded events |
//! | `ARSY-CMP-1000` | the session store could not be opened or written |
//! | `ARSY-CFG-1000` | a configuration layer could not be read or does not parse |
//! | `ARSY-PRV-1000` | no provider credential is available, so the turn cannot dispatch |
//! | `ARSY-PRV-1002` | an installed provider CLI failed |
//! | `ARSY-SBX-1000` | no sandbox worker is available on this build |
//! | `ARSY-PRV-1001` | no credential store is registered |
//! | `ARSY-UIX-1000` | interactive terminal input or output failed |

mod acp;
#[cfg(feature = "tui")]
mod approval;
mod code;
mod config_edit;
mod connector;
mod eval;
mod evidence;
mod extensions;
mod integrations;
mod mcp;
mod memory;
mod policy;
mod progress;
pub mod provider;
mod review;
mod serve;
mod session;
mod subagent;
mod telemetry;
mod transcript;
#[cfg(feature = "tui")]
pub mod tui;

use arsy_kernel::{
    artifact::unix_time_ms,
    config::Config,
    domain::{AgentId, Principal, SessionId, TaskId},
    event::EventStore,
    orchestration::{Budget, TaskGraph, TaskNode, TaskState, WorkspaceRequirement},
    protocol::{ClientRequest, Extensions, IdempotencyKey, ProtocolEnvelope, TurnStart},
    provider::{
        CanonicalModelRequest, ModelContent, ModelEvent, ModelKey, ModelMessage, ModelProvider,
        ModelRole, ProviderError,
    },
    secret::{
        CredentialStore, FileCredentialStore, OsCredentialStore, Redactor, SecretBroker,
        SecretError, SecretHandle, FILE_STORE_ID, OS_STORE_ID,
    },
    service::AgentService,
    sqlite::{Durability, SqliteEventStore},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Every session of one workspace shares this store.
const STORE_PATH: &str = ".arsy/sessions.sqlite3";

/// No provider credential is available, so the turn cannot dispatch.
pub const ARSY_PRV_1000: &str = "ARSY-PRV-1000";
/// A configuration layer could not be read or does not parse.
pub const ARSY_CFG_1000: &str = "ARSY-CFG-1000";
/// Machine records carry the protocol's schema version.
const RECORD_SCHEMA: u32 = 1;

/// Documented commands that a later phase ships, so they report their phase
/// instead of failing as unknown input.
const UNAVAILABLE: &[(&str, u8)] = &[("completions", 1)];

const USAGE: &str = "\
arsy — agentic coding harness

Usage:
  arsy run <TASK> [--image <PATH>]   execute one task non-interactively
                             ('-' reads the task from stdin)
  arsy resume <SESSION_ID>   resume a recorded session
  arsy doctor                report platform, sandbox, credential, and config state
  arsy eval <SUITE> [--strict]  run an evaluation fixture; --strict needs its revision
  arsy compat explain <KIND> explain claude, codex, omp, or agents imports
  arsy config explain [KEY]  show effective configuration and where it came from
  arsy session list [--limit <N>]      list recorded sessions in this workspace
  arsy session show <ID> [--turns] [--evidence]   show one session's projection
  arsy session export <ID> [--out <PATH>]         export canonical events as JSONL
  arsy session rewind <ID> --to <EVENT_ID>        branch continuing from an event
  arsy session fork <ID> [--at <EVENT_ID>]        branch recording ancestry only
  arsy artifact show <REF> [--max-bytes <N>]      render a bounded, redacted excerpt
  arsy artifact export <REF> --out <PATH>         write one artifact to a file
  arsy gc [--apply] [--retention <DURATION>]      report, then remove, unreachable evidence
  arsy migrate [--apply] [--backup <PATH>]        report, then apply, the store's schema migration
  arsy memory list [--scope <SCOPE>] [--all]      what this workspace remembers
  arsy memory remember <CLAIM> [--scope <SCOPE>]  record a durable claim
  arsy memory forget <ID> [--to <REASON>]        withdraw one, keeping the tombstone
  arsy code symbol <NAME> [--tier auto|text]      where a name is declared
  arsy code explain|references <SYMBOL_ID>        what it is, and what it affects
  arsy code diagnostics <PATH>                    what a language server sees
  arsy review [REVISION] [--strict]  report what changed since REVISION (default HEAD)
  arsy policy explain <OPERATION> [--resource <REF>] [--actor <ID>]
  arsy skill list [--source <ECOSYSTEM>]          declared skills (data only)
  arsy plugin list [--capabilities]               installed plugins
  arsy plugin install <SOURCE> [--force]          approve, then install
  arsy plugin inspect <ID> | arsy plugin remove <ID>
  arsy plugin run <ID> [--to <INPUT>]              invoke an installed plugin
  arsy plugin refresh [ID] [--dry-run]            re-read plugin sources
  arsy serve [--protocol mcp|acp]                offer operations as MCP tools, or
                                                 speak ACP to an editor, on stdio
  arsy provider list [--all]                      providers resolved as allowed
  arsy model list [--provider <ID>] [--capability <NAME>]
  arsy mcp list [--source <KIND>]       inspect imported MCP declarations
  arsy mcp show <NAME> [--source <KIND>] show one MCP declaration
  arsy mcp add <NAME> --command <CMD> [-- ARGS...]  define a stdio connection
  arsy mcp add <NAME> --transport http --url <URL>  define an HTTP connection
  arsy mcp remove|enable|disable <NAME> [--scope <user|workspace>]
  arsy mcp test <NAME> [--timeout <SECONDS>]  connect, negotiate, disconnect
  arsy hook list [--event <NAME>]      list lifecycle hooks and what runs
  arsy auth set <PROVIDER>   store a credential in the OS credential store
  arsy auth login <PROVIDER> sign in to a provider through its OAuth client
  arsy auth list             list credential handles (never values)
  arsy auth remove <HANDLE>  remove a credential from the OS credential store
  arsy update [--check]      report the running version; ARSY does not self-update

Global flags:
  --workspace <PATH>   workspace root (default: current directory)
  --config <PATH>      one extra config file, applied last; it cannot widen policy
  --provider <ID>      the endpoint this run dispatches to
  --model <ID>         the model this run asks for, within model.allowed
  --output <MODE>      human, json, or ci
  --no-color           disable ANSI styling
  --debug              trace the agent loop to stderr as JSON lines:
                       requests, normalized model events, tool results, retries
  --help, --version
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Human,
    Json,
    Ci,
    /// Records become ACP `session/update` notifications on the protocol's own
    /// stdout. Not selectable with `--output`: it is what `arsy serve
    /// --protocol acp` installs for the turn it is serving.
    Acp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Warning,
    Error,
}

impl Severity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub remediation: String,
}

impl Diagnostic {
    pub fn error(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            severity: Severity::Error,
            message: message.into(),
            remediation: remediation.into(),
        }
    }

    pub fn warning(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message, remediation)
        }
    }

    /// Exit code selected by the diagnostic class, per `docs/36-cli-tui.md`.
    /// An unrecognized class is treated as invalid input rather than success.
    pub fn exit_code(&self) -> i32 {
        match self.code.split('-').nth(1).unwrap_or_default() {
            "POL" => 3,
            "SBX" => 4,
            "PRV" | "PRT" => 5,
            "EXE" | "TLS" | "EDT" | "STL" => 6,
            "VER" => 7,
            "CMP" | "CRD" => 8,
            "RET" | "CTX" | "PLN" | "MDL" => 9,
            "UIX" => 10,
            _ => 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Run {
        task: String,
        /// `--image <PATH>`: a picture attached to the prompt.
        image: Option<PathBuf>,
    },
    Resume {
        session: SessionId,
        follow: bool,
    },
    Doctor {
        strict: bool,
    },
    Update {
        check_only: bool,
    },
    AuthSet {
        provider: String,
        handle: Option<String>,
    },
    /// `arsy auth login <PROVIDER>`: sign in through the provider's OAuth
    /// client instead of storing an API key.
    AuthLogin {
        provider: String,
    },
    AuthList,
    AuthRemove {
        handle: SecretHandle,
        force: bool,
    },
    Eval {
        suite: PathBuf,
        trials: Option<u32>,
        /// Refuse to run unless the workspace is at the revision the fixture
        /// pins, for a pipeline that needs its numbers to be comparable.
        strict: bool,
        out: Option<PathBuf>,
    },
    CompatExplain {
        ecosystem: arsy_code::compat::Ecosystem,
    },
    Inspect {
        kind: String,
        name: Option<String>,
        source: Option<String>,
        event: Option<String>,
    },
    /// `arsy config explain [KEY]`: effective values and where each came from.
    ConfigExplain {
        key: Option<String>,
    },
    SessionList {
        limit: usize,
    },
    SessionShow {
        session: SessionId,
        turns: bool,
        evidence: bool,
    },
    SessionExport {
        session: SessionId,
        out: Option<PathBuf>,
        include_artifacts: bool,
    },
    SessionDelete {
        session: SessionId,
    },
    SessionRename {
        session: SessionId,
        title: String,
    },
    /// `arsy session rewind` and `arsy session fork`: one operation, two
    /// documented names, distinguished by whether the branch inherits the
    /// parent's prefix.
    SessionBranch {
        session: SessionId,
        at: Option<arsy_kernel::domain::EventId>,
        mode: arsy_kernel::service::BranchMode,
    },
    ArtifactShow {
        reference: arsy_kernel::domain::ArtifactId,
        max_bytes: u64,
    },
    ArtifactExport {
        reference: arsy_kernel::domain::ArtifactId,
        out: PathBuf,
    },
    Gc {
        apply: bool,
        retention_ms: u64,
    },
    MemoryList {
        scope: Option<String>,
        all: bool,
    },
    MemoryRemember {
        claim: String,
        scope: Option<String>,
    },
    MemoryForget {
        id: arsy_kernel::domain::MemoryId,
        reason: String,
    },
    CodeSymbol {
        name: String,
        tier: code::Tier,
        limit: Option<usize>,
    },
    CodeInspect {
        /// `code.explain` or `code.references`; one shape, two questions.
        operation: &'static str,
        symbol: String,
    },
    CodeDiagnostics {
        path: String,
    },
    /// `arsy review`: assess what the working tree changed.
    Review {
        /// What the working tree is compared against. `HEAD` by default.
        base: String,
        /// Any finding becomes a non-zero exit, for a pipeline gate.
        strict: bool,
    },
    /// `arsy migrate`: move the session store to the supported schema version.
    Migrate {
        apply: bool,
        /// Where the pre-migration copy goes; defaults beside the store.
        backup: Option<PathBuf>,
    },
    /// `arsy policy explain <OPERATION>`: evaluate without executing.
    PolicyExplain {
        operation: String,
        resource: Option<String>,
        actor: Option<String>,
    },
    ProviderList {
        all: bool,
    },
    ModelList {
        provider: Option<String>,
        capability: Option<String>,
    },
    McpImport {
        scope: mcp::Scope,
    },
    McpAdd {
        server: arsy_kernel::config::McpServer,
        scope: mcp::Scope,
    },
    McpRemove {
        name: String,
        scope: mcp::Scope,
    },
    /// `arsy mcp enable` and `arsy mcp disable`: one key, two names.
    McpEnable {
        name: String,
        enabled: bool,
        scope: mcp::Scope,
    },
    McpTest {
        name: String,
        /// `None` keeps the connection's configured deadline.
        timeout_ms: Option<u64>,
    },
    SkillList {
        source: Option<String>,
    },
    PluginList {
        capabilities: bool,
    },
    PluginInstall {
        source: PathBuf,
        force: bool,
    },
    PluginInspect {
        id: String,
    },
    /// `arsy plugin run <ID>`: invoke an installed plugin through the operation
    /// registry, so the same policy and audit trail apply as to any other call.
    PluginRun {
        id: String,
        /// What the plugin is given, as text. `--to` carries it.
        input: String,
    },
    PluginRemove {
        id: String,
    },
    PluginRefresh {
        id: Option<String>,
        dry_run: bool,
    },
    /// `arsy serve`: speak MCP on stdio for an embedding client.
    Serve,
    /// `arsy serve --protocol acp`: speak an editor's session vocabulary.
    ServeAcp,
    /// Bare `arsy`: the interactive TUI.
    Tui,
    Help,
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub workspace: PathBuf,
    pub output: Option<Output>,
    /// `--no-color`; `NO_COLOR` in the environment disables styling as well.
    pub no_color: bool,
    /// `--debug`: trace the agent loop — normalized model events, tool-loop
    /// transitions, request metadata, retry decisions — to stderr as JSON.
    pub debug: bool,
    /// `--config <PATH>`: one extra configuration file, applied after every
    /// discovered layer. It cannot weaken policy — ceilings intersect.
    pub config: Option<PathBuf>,
    /// `--provider <ID>`: the endpoint a turn dispatches to, overriding
    /// `provider.default` and the routing that "auto" would otherwise do.
    pub provider: Option<String>,
    /// `--model <ID>`: the model a turn asks for, overriding the endpoint's
    /// own `model` and `model.default`.
    pub model: Option<String>,
    pub command: Command,
}

/// Parse arguments without touching the filesystem or starting a session.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, Diagnostic> {
    let mut parsed = collect_arguments(args)?;
    // Taken before the per-command parse, which moves the rest of `parsed`.
    let global = Global {
        workspace: parsed.workspace.take(),
        output: parsed.output,
        no_color: parsed.no_color,
        debug: parsed.debug,
        config: parsed.config.take(),
        // `--provider` doubles as the filter for `arsy model list`, so it is
        // cloned rather than taken: one flag, read in both places.
        provider: parsed.provider.clone(),
        model: parsed.model.clone(),
    };
    if let Some(command) = parsed.early.take() {
        return Ok(invocation(global, command));
    }
    if (parsed.source.is_some() || parsed.event.is_some())
        && !matches!(parsed.name.as_deref(), Some("mcp" | "hook" | "skill"))
    {
        return Err(usage(
            "--source and --event apply only to MCP, hook, and skill inspection",
        ));
    }
    let command = match parsed.name.as_deref() {
        None => Command::Tui,
        Some("run") => Command::Run {
            task: only_argument(parsed.positional, "run", "<TASK>")?,
            image: parsed.image.take(),
        },
        Some("resume") => parse_resume(parsed.positional, parsed.follow)?,
        Some("doctor") => parse_doctor(parsed.positional, parsed.strict)?,
        Some("update") => Command::Update {
            check_only: parsed.check,
        },
        Some("eval") => Command::Eval {
            suite: PathBuf::from(only_argument(parsed.positional, "eval", "<SUITE>")?),
            trials: parsed.trials,
            strict: parsed.strict,
            out: parsed.out,
        },
        Some("compat") => Command::CompatExplain {
            ecosystem: compatibility_kind(parsed.positional)?,
        },
        Some("session") => session::parse(&parsed)?,
        Some("artifact") => evidence::parse_artifact(&parsed)?,
        Some("gc") => evidence::parse_gc(&parsed)?,
        Some("migrate") => session::parse_migrate(&parsed)?,
        Some("review") => review::parse(&parsed)?,
        Some("code") => code::parse(&parsed)?,
        Some("memory") => memory::parse(&parsed)?,
        Some("policy") => policy::parse(&parsed)?,
        Some("serve") => serve::parse(&parsed)?,
        Some("skill") => extensions::parse_skill(&parsed)?,
        Some("plugin") => extensions::parse_plugin(&parsed)?,
        Some("provider") => provider::parse_list(&parsed)?,
        Some("model") => provider::parse_models(&parsed)?,
        Some("auth") => parse_auth(parsed.positional, parsed.handle, parsed.force)?,
        Some("config") => parse_config(parsed.positional)?,
        Some("mcp") => mcp::parse(&parsed)?,
        Some("hook") => {
            integrations::parse("hook", parsed.positional, parsed.source, parsed.event)?
        }
        Some(other) => return Err(unknown_command(other)),
    };
    Ok(invocation(global, command))
}

#[derive(Default)]
struct ParsedArguments {
    workspace: Option<PathBuf>,
    output: Option<Output>,
    no_color: bool,
    check: bool,
    follow: bool,
    strict: bool,
    force: bool,
    turns: bool,
    evidence: bool,
    include_artifacts: bool,
    apply: bool,
    all: bool,
    capabilities: bool,
    dry_run: bool,
    /// `--debug`: trace the agent loop to stderr as JSON lines.
    debug: bool,
    handle: Option<String>,
    trials: Option<u32>,
    limit: Option<usize>,
    max_bytes: Option<u64>,
    out: Option<PathBuf>,
    source: Option<String>,
    event: Option<String>,
    to: Option<String>,
    at: Option<String>,
    retention: Option<String>,
    /// `arsy code symbol --tier`: which tier answers.
    tier: Option<String>,
    /// `arsy migrate --backup`: where the pre-migration copy goes.
    backup: Option<PathBuf>,
    /// `arsy review --base`: the revision the working tree is compared against.
    base: Option<String>,
    resource: Option<String>,
    actor: Option<String>,
    /// `--config <PATH>`: one extra configuration file for this invocation.
    config: Option<PathBuf>,
    /// `--image <PATH>`: a picture attached to the prompt.
    image: Option<PathBuf>,
    /// `--provider <ID>`: a global override, and the filter `model list` reads.
    provider: Option<String>,
    /// `--model <ID>`: a global override of the model a turn asks for.
    model: Option<String>,
    capability: Option<String>,
    transport: Option<String>,
    protocol: Option<String>,
    command: Option<String>,
    url: Option<String>,
    scope: Option<String>,
    timeout: Option<u64>,
    name: Option<String>,
    positional: Vec<String>,
    early: Option<Command>,
}

fn collect_arguments<I: IntoIterator<Item = String>>(
    args: I,
) -> Result<ParsedArguments, Diagnostic> {
    let mut arguments = args.into_iter();
    let mut parsed = ParsedArguments::default();

    while let Some(argument) = arguments.next() {
        // Everything after a bare `--` belongs to whatever the command is
        // wrapping, so a subprocess's own flags cannot be mistaken for ARSY's.
        if argument == "--" {
            parsed.positional.extend(arguments.by_ref());
            break;
        }
        if apply_switch(&argument, &mut parsed) {
            if parsed.early.is_some() {
                return Ok(parsed);
            }
            continue;
        }
        if apply_value_flag(&argument, &mut parsed, &mut arguments)? {
            continue;
        }
        if argument.starts_with("--") {
            return Err(usage(format!("unknown flag {argument}")));
        }
        if parsed.name.is_none() {
            parsed.name = Some(argument);
        } else {
            parsed.positional.push(argument);
        }
    }
    Ok(parsed)
}

fn apply_switch(argument: &str, parsed: &mut ParsedArguments) -> bool {
    match argument {
        "--help" | "-h" => parsed.early = Some(Command::Help),
        "--version" | "-V" => parsed.early = Some(Command::Version),
        "--no-color" => parsed.no_color = true,
        "--check" => parsed.check = true,
        "--follow" => parsed.follow = true,
        "--strict" => parsed.strict = true,
        "--force" => parsed.force = true,
        "--turns" => parsed.turns = true,
        "--evidence" => parsed.evidence = true,
        "--include-artifacts" => parsed.include_artifacts = true,
        "--apply" => parsed.apply = true,
        "--all" => parsed.all = true,
        "--capabilities" => parsed.capabilities = true,
        "--dry-run" => parsed.dry_run = true,
        "--debug" => parsed.debug = true,
        _ => return false,
    }
    true
}

fn apply_value_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    match argument {
        "--workspace" => parsed.workspace = Some(PathBuf::from(value(arguments, argument)?)),
        "--output" => parsed.output = Some(output_mode(&value(arguments, argument)?)?),
        "--handle" => parsed.handle = Some(value(arguments, argument)?),
        "--trials" => {
            parsed.trials = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--trials must be an integer"))?,
            );
        }
        "--limit" => {
            parsed.limit = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--limit must be a non-negative integer"))?,
            );
        }
        "--max-bytes" => {
            parsed.max_bytes = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--max-bytes must be a non-negative integer"))?,
            );
        }
        "--out" => parsed.out = Some(PathBuf::from(value(arguments, argument)?)),
        "--source" => parsed.source = Some(value(arguments, argument)?),
        "--event" => parsed.event = Some(value(arguments, argument)?),
        "--to" => parsed.to = Some(value(arguments, argument)?),
        "--at" => parsed.at = Some(value(arguments, argument)?),
        "--retention" => parsed.retention = Some(value(arguments, argument)?),
        "--backup" => parsed.backup = Some(PathBuf::from(value(arguments, argument)?)),
        "--tier" => parsed.tier = Some(value(arguments, argument)?),
        "--base" => parsed.base = Some(value(arguments, argument)?),
        "--resource" => parsed.resource = Some(value(arguments, argument)?),
        "--actor" => parsed.actor = Some(value(arguments, argument)?),
        "--config" => parsed.config = Some(PathBuf::from(value(arguments, argument)?)),
        "--image" => parsed.image = Some(PathBuf::from(value(arguments, argument)?)),
        "--provider" => parsed.provider = Some(value(arguments, argument)?),
        "--model" => parsed.model = Some(value(arguments, argument)?),
        "--capability" => parsed.capability = Some(value(arguments, argument)?),
        "--transport" => parsed.transport = Some(value(arguments, argument)?),
        "--protocol" => parsed.protocol = Some(value(arguments, argument)?),
        "--command" => parsed.command = Some(value(arguments, argument)?),
        "--url" => parsed.url = Some(value(arguments, argument)?),
        "--scope" => parsed.scope = Some(value(arguments, argument)?),
        "--timeout" => {
            parsed.timeout = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--timeout must be a whole number of seconds"))?,
            );
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn unknown_command(command: &str) -> Diagnostic {
    match UNAVAILABLE.iter().find(|(known, _)| *known == command) {
        Some((_, phase)) => Diagnostic::error(
            "ARSY-SCH-1002",
            format!("`arsy {command}` is not available yet"),
            format!("it ships in phase {phase}; see docs/36-cli-tui.md"),
        ),
        None => Diagnostic::error(
            "ARSY-SCH-1000",
            format!("unknown command `{command}`"),
            "run `arsy --help` for the available commands",
        ),
    }
}

/// The flags every command accepts, lifted out of the per-command parse.
#[derive(Default)]
struct Global {
    workspace: Option<PathBuf>,
    output: Option<Output>,
    no_color: bool,
    debug: bool,
    config: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
}

fn invocation(global: Global, command: Command) -> Invocation {
    Invocation {
        workspace: global.workspace.unwrap_or_else(|| PathBuf::from(".")),
        output: global.output,
        no_color: global.no_color,
        debug: global.debug,
        config: global.config,
        provider: global.provider,
        model: global.model,
        command,
    }
}

fn compatibility_kind(
    mut positional: Vec<String>,
) -> Result<arsy_code::compat::Ecosystem, Diagnostic> {
    if positional.first().map(String::as_str) != Some("explain") {
        return Err(usage("compat requires `explain <claude|codex|omp|agents>`"));
    }
    positional.remove(0);
    match only_argument(positional, "compat explain", "<KIND>")?.as_str() {
        "claude" => Ok(arsy_code::compat::Ecosystem::Claude),
        "codex" => Ok(arsy_code::compat::Ecosystem::Codex),
        "omp" => Ok(arsy_code::compat::Ecosystem::Omp),
        "agents" => Ok(arsy_code::compat::Ecosystem::AgentsMd),
        other => Err(usage(format!("unsupported compatibility kind `{other}`"))),
    }
}

fn parse_resume(positional: Vec<String>, follow: bool) -> Result<Command, Diagnostic> {
    let id = only_argument(positional, "resume", "<SESSION_ID>")?;
    let session = id
        .parse()
        .map_err(|_| usage(format!("`{id}` is not a canonical session ID")))?;
    Ok(Command::Resume { session, follow })
}

fn parse_doctor(positional: Vec<String>, strict: bool) -> Result<Command, Diagnostic> {
    if !positional.is_empty() {
        return Err(usage("doctor takes no positional argument"));
    }
    Ok(Command::Doctor { strict })
}

/// Slash commands that are an existing CLI inspection under another name: the
/// argv they expand to, and the subcommand to assume when the line carries only
/// flags. One table, so the composer cannot offer a command the loop below does
/// not know how to run.
///
/// Every entry is read-only. `auth` expands to `auth list` with the user's words
/// appended, so `set`, `login`, and `remove` cannot be reached from the TUI:
/// they fail to parse instead of touching stored credentials.
#[cfg(feature = "tui")]
const INSPECTIONS: &[(&str, &[&str], Option<&str>)] = &[
    ("/mcp", &["mcp"], Some("list")),
    ("/hooks", &["hook"], Some("list")),
    ("/settings", &["config", "explain"], None),
    ("/doctor", &["doctor"], None),
    ("/auth", &["auth"], Some("list")),
    ("/compat", &["compat", "explain"], None),
];

/// Expand a typed slash line into CLI argv, or `None` when no inspection owns
/// it. The line is not validated here — `parse` already rejects a bad argument
/// with the same diagnostic the CLI would give.
#[cfg(feature = "tui")]
fn inspection_args(line: &str) -> Option<Vec<String>> {
    let mut words = line.split_whitespace();
    let command = words.next()?;
    let (_, prefix, default) = INSPECTIONS.iter().find(|(name, _, _)| *name == command)?;
    let mut args: Vec<String> = prefix.iter().map(|word| (*word).to_owned()).collect();
    let rest: Vec<String> = words.map(str::to_owned).collect();
    if rest.first().is_none_or(|word| word.starts_with("--")) {
        args.extend(default.map(str::to_owned));
    }
    args.extend(rest);
    Some(args)
}

/// `config explain [KEY]`. Only `explain` exists; the rest of the documented
/// `config` surface belongs to a later phase.
fn parse_config(positional: Vec<String>) -> Result<Command, Diagnostic> {
    match positional.first().map(String::as_str) {
        Some("explain") if positional.len() <= 2 => Ok(Command::ConfigExplain {
            key: positional.into_iter().nth(1),
        }),
        Some("explain") => Err(usage("config explain accepts only [KEY]")),
        _ => Err(usage("config requires explain")),
    }
}

fn parse_auth(
    mut positional: Vec<String>,
    handle: Option<String>,
    force: bool,
) -> Result<Command, Diagnostic> {
    match positional.first().map(String::as_str) {
        Some("set") => {
            positional.remove(0);
            Ok(Command::AuthSet {
                provider: only_argument(positional, "auth set", "<PROVIDER>")?,
                handle,
            })
        }
        Some("login") => {
            positional.remove(0);
            Ok(Command::AuthLogin {
                provider: only_argument(positional, "auth login", "<PROVIDER>")?,
            })
        }
        Some("list") if positional.len() == 1 => Ok(Command::AuthList),
        Some("remove") => {
            positional.remove(0);
            let raw = only_argument(positional, "auth remove", "<HANDLE>")?;
            Ok(Command::AuthRemove {
                handle: raw.try_into().map_err(secret_failed)?,
                force,
            })
        }
        _ => Err(usage("auth requires set, login, list, or remove")),
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Diagnostic> {
    arguments
        .next()
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| usage(format!("{flag} requires a value")))
}

fn output_mode(value: &str) -> Result<Output, Diagnostic> {
    match value {
        "human" => Ok(Output::Human),
        "json" => Ok(Output::Json),
        "ci" => Ok(Output::Ci),
        other => Err(usage(format!(
            "invalid --output `{other}`, expected human, json, or ci"
        ))),
    }
}

fn only_argument(
    mut positional: Vec<String>,
    command: &str,
    expected: &str,
) -> Result<String, Diagnostic> {
    match positional.len() {
        1 => Ok(positional.remove(0)),
        0 => Err(usage(format!("{command} requires {expected}"))),
        _ => Err(usage(format!("{command} accepts only {expected}"))),
    }
}

fn usage(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(
        "ARSY-SCH-1001",
        message,
        "run `arsy --help` for the surface",
    )
}

/// Emits the records of one invocation in the selected output mode.
struct Emitter {
    output: Output,
    session: Option<SessionId>,
    sequence: u64,
    /// `--debug`: trace the agent loop to stderr.
    debug: bool,
    redactor: Redactor,
    /// Whether streamed text is mid-line, so the next output can start clean.
    streaming: bool,
}

impl Emitter {
    const fn new(output: Output) -> Self {
        Self {
            output,
            session: None,
            sequence: 0,
            debug: false,
            redactor: Redactor::new(),
            streaming: false,
        }
    }

    const fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic) {
        let message = self
            .redactor
            .sanitize(&diagnostic.message)
            .unwrap_or_else(|_| "output suppressed by secret redaction".to_owned());
        let remediation = self
            .redactor
            .sanitize(&diagnostic.remediation)
            .unwrap_or_else(|_| "output suppressed by secret redaction".to_owned());
        match self.output {
            Output::Json => self.record(
                "diagnostic",
                json!({
                    "code": diagnostic.code,
                    "severity": diagnostic.severity.as_str(),
                    "message": message,
                    "remediation": remediation,
                }),
            ),
            Output::Acp => acp::notify(
                self.session,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": format!("{}: {message}", diagnostic.code)},
                }),
            ),
            // Both human and CI keep diagnostics on stderr; the CI form is the
            // stable, unlocalized one machines grep for.
            _ => {
                let severity = diagnostic.severity.as_str().to_uppercase();
                let _ = writeln!(
                    io::stderr(),
                    "ARSY {severity} {} {}\n  {}",
                    diagnostic.code,
                    terminal_text(&message),
                    terminal_text(&remediation)
                );
            }
        }
    }

    fn result(&mut self, payload: Value) {
        let payload = match self.redactor.sanitize(&payload.to_string()) {
            Ok(sanitized) => serde_json::from_str(&sanitized).unwrap_or(Value::String(sanitized)),
            Err(_) => json!({"error": "output suppressed by secret redaction"}),
        };
        match self.output {
            Output::Json => self.record("result", payload),
            // The turn's outcome is the JSON-RPC response the serve loop
            // sends; repeating it as a notification would report it twice.
            Output::Acp => {}
            _ => {
                let mut stdout = io::stdout();
                if let Some(fields) = payload.as_object() {
                    for (key, value) in fields {
                        let _ = writeln!(stdout, "{key}: {}", plain(value));
                    }
                }
            }
        }
    }

    /// One step of the agent loop, for an operator who is debugging it.
    ///
    /// # Why stderr, and why JSON either way
    ///
    /// A trace is diagnostics, not output: a pipeline reading `--output json`
    /// on stdout must not have its records interleaved with loop internals, and
    /// a human watching a turn must not have their reply buried in them. So it
    /// goes to stderr, where it can be redirected away or captured on its own.
    ///
    /// It is JSON in both output modes rather than prose in one: the reason to
    /// turn this on is to grep it, and a format that changed with `--output`
    /// would mean writing the grep twice.
    ///
    /// Off by default and a no-op when off, so the normal loop stays exactly as
    /// concise as it was.
    fn trace(&mut self, event: &str, fields: Value) {
        if !self.debug {
            return;
        }
        self.sequence += 1;
        let record = json!({
            "schema_version": RECORD_SCHEMA,
            "type": "debug",
            "event": event,
            "sequence": self.sequence,
            "session_id": self.session.map(|session| session.to_string()),
            "payload": fields,
        });
        // Redacted like every other sink: a debug mode that printed the
        // credential an ordinary record would have masked would be the most
        // dangerous flag in the binary.
        if let Ok(record) = self.redactor.sanitize(&record.to_string()) {
            let _ = writeln!(io::stderr(), "{record}");
        }
    }

    fn record(&mut self, kind: &str, payload: Value) {
        self.sequence += 1;
        let record = json!({
            "schema_version": RECORD_SCHEMA,
            "type": kind,
            "sequence": self.sequence,
            "session_id": self.session.map(|session| session.to_string()),
            "payload": payload,
        });
        if let Ok(record) = self.redactor.sanitize(&record.to_string()) {
            let _ = writeln!(io::stdout(), "{record}");
        }
    }

    /// One chunk of streamed model text.
    ///
    /// Human output writes it straight through so a reply appears as it is
    /// produced; machine output makes each chunk its own record, because a
    /// JSON-lines consumer cannot read a partial line.
    fn delta(&mut self, text: &str) {
        match self.output {
            Output::Json => self.record("model.delta", json!({"text": text})),
            Output::Acp => {
                if let Ok(text) = self.redactor.sanitize(text) {
                    acp::notify(
                        self.session,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": text},
                        }),
                    );
                }
            }
            _ => {
                let Ok(text) = self.redactor.sanitize(text) else {
                    return;
                };
                let mut stdout = io::stdout();
                let _ = write!(stdout, "{text}").and_then(|()| stdout.flush());
                self.streaming = true;
            }
        }
    }

    /// Close a run of streamed text so the next line starts on its own.
    fn end_deltas(&mut self) {
        if std::mem::take(&mut self.streaming) {
            let _ = writeln!(io::stdout());
        }
    }

    fn install_redactor(&mut self, redactor: Redactor) {
        self.redactor = redactor;
    }
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => terminal_text(text),
        other => other.to_string(),
    }
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Parse and execute one invocation, returning the process exit code.
pub fn run_cli<I: IntoIterator<Item = String>>(args: I, tty: bool) -> i32 {
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(diagnostic) => {
            // The output mode is not established yet, so this stays on stderr.
            Emitter::new(Output::Human).diagnostic(&diagnostic);
            return diagnostic.exit_code();
        }
    };
    let output = invocation
        .output
        .unwrap_or(if tty { Output::Human } else { Output::Ci });
    let mut emitter = Emitter::new(output).with_debug(invocation.debug);
    match execute(&invocation, tty, &mut emitter) {
        Ok(code) => code,
        Err(diagnostic) => {
            emitter.diagnostic(&diagnostic);
            if emitter.output == Output::Json {
                emitter.result(json!({"status": "failed", "code": diagnostic.code}));
            }
            diagnostic.exit_code()
        }
    }
}

fn execute(invocation: &Invocation, tty: bool, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    // Grouped by area rather than one flat dispatch: a reader after the
    // session commands should not have to walk the credential ones to find
    // them. The core arms come last because the interactive one needs to know
    // whether it has a terminal, which none of the others care about.
    execute_auth(invocation, tty, emitter)
        .or_else(|| execute_session(invocation, emitter))
        .or_else(|| execute_memory(invocation, emitter))
        .or_else(|| execute_mcp(invocation, emitter))
        .or_else(|| execute_code(invocation, tty, emitter))
        .unwrap_or_else(|| execute_core(invocation, tty, emitter))
}

/// Credentials: what ARSY holds and for which provider.
fn execute_auth(
    invocation: &Invocation,
    tty: bool,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::AuthSet { provider, handle } => {
            auth_set(invocation, provider, handle.as_deref(), tty, emitter)
        }
        Command::AuthLogin { provider } => auth_login(invocation, provider, emitter),
        Command::AuthList => auth_list(invocation, emitter),
        Command::AuthRemove { handle, force } => auth_remove(invocation, handle, *force, emitter),
        _ => return None,
    })
}

/// Recorded sessions and the artifacts they produced.
fn execute_session(
    invocation: &Invocation,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::SessionList { limit } => session::list(invocation, *limit, emitter),
        Command::SessionShow {
            session,
            turns,
            evidence,
        } => session::show(invocation, *session, *turns, *evidence, emitter),
        Command::SessionExport {
            session,
            out,
            include_artifacts,
        } => session::export(
            invocation,
            *session,
            out.as_deref(),
            *include_artifacts,
            emitter,
        ),
        Command::SessionDelete { session } => session::delete(invocation, *session, emitter),
        Command::SessionRename { session, title } => {
            session::rename(invocation, *session, title, emitter)
        }
        Command::SessionBranch { session, at, mode } => {
            session::branch(invocation, *session, *at, *mode, emitter)
        }
        Command::ArtifactShow {
            reference,
            max_bytes,
        } => evidence::show(invocation, *reference, *max_bytes, emitter),
        Command::ArtifactExport { reference, out } => {
            evidence::export(invocation, *reference, out, emitter)
        }
        _ => return None,
    })
}

/// The durable claims a workspace carries between sessions.
fn execute_memory(
    invocation: &Invocation,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::MemoryList { scope, all } => {
            memory::list(invocation, scope.clone(), *all, emitter)
        }
        Command::MemoryRemember { claim, scope } => {
            memory::remember(invocation, claim, scope.clone(), emitter)
        }
        Command::MemoryForget { id, reason } => memory::forget(invocation, *id, reason, emitter),
        _ => return None,
    })
}

/// MCP connection definitions.
fn execute_mcp(invocation: &Invocation, emitter: &mut Emitter) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::McpAdd { server, scope } => mcp::add(invocation, server, *scope, emitter),
        Command::McpImport { scope } => mcp::import(invocation, *scope, emitter),
        Command::McpRemove { name, scope } => mcp::remove(invocation, name, *scope, emitter),
        Command::McpEnable {
            name,
            enabled,
            scope,
        } => mcp::set_enabled(invocation, name, *enabled, *scope, emitter),
        Command::McpTest { name, timeout_ms } => mcp::test(invocation, name, *timeout_ms, emitter),
        _ => return None,
    })
}

/// Reading code: symbols, diagnostics, and the extension surface.
fn execute_code(
    invocation: &Invocation,
    tty: bool,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::CodeSymbol { name, tier, limit } => {
            code::symbol(invocation, name, *tier, *limit, emitter)
        }
        Command::CodeInspect { operation, symbol } => {
            code::inspect(invocation, operation, symbol, emitter)
        }
        Command::CodeDiagnostics { path } => code::diagnostics(invocation, path, emitter),
        Command::PluginList { capabilities } => {
            extensions::list(invocation, *capabilities, emitter)
        }
        Command::PluginInstall { source, force } => {
            extensions::install(invocation, source, *force, tty, emitter)
        }
        Command::PluginInspect { id } => extensions::inspect(invocation, id, emitter),
        Command::PluginRun { id, input } => extensions::run(invocation, id, input, emitter),
        Command::PluginRemove { id } => extensions::remove(invocation, id, emitter),
        Command::PluginRefresh { id, dry_run } => {
            extensions::refresh(invocation, id.as_deref(), *dry_run, emitter)
        }
        _ => return None,
    })
}

/// Report the integrations a workspace declares.
#[allow(clippy::too_many_arguments)]
fn inspect_report(
    invocation: &Invocation,
    kind: &str,
    name: Option<&str>,
    source: Option<&str>,
    event: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let report = integrations::inspect(
        &workspace_root(&invocation.workspace)?,
        kind,
        name,
        source,
        event,
        invocation.config.as_deref(),
    )?;
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        integrations::human_report(&report, kind, source, event)
    });
    Ok(0)
}

/// Read-only questions about the workspace and what ARSY resolved for it.
fn execute_inspect(
    invocation: &Invocation,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::Doctor { strict } => Ok(doctor(invocation, *strict, emitter)),
        Command::CompatExplain { ecosystem } => compat_explain(invocation, *ecosystem, emitter),
        Command::Inspect {
            kind,
            name,
            source,
            event,
        } => inspect_report(
            invocation,
            kind,
            name.as_deref(),
            source.as_deref(),
            event.as_deref(),
            emitter,
        ),
        Command::ConfigExplain { key } => config_explain(invocation, key.as_deref(), emitter),
        Command::Review { base, strict } => review::run(invocation, base, *strict, emitter),
        Command::PolicyExplain {
            operation,
            resource,
            actor,
        } => policy::explain(
            invocation,
            operation,
            resource.as_deref(),
            actor.as_deref(),
            emitter,
        ),
        Command::SkillList { source } => extensions::skills(invocation, source.as_deref(), emitter),
        Command::ProviderList { all } => provider::list(invocation, *all, emitter),
        Command::ModelList {
            provider,
            capability,
        } => provider::models(
            invocation,
            provider.as_deref(),
            capability.as_deref(),
            emitter,
        ),
        _ => return None,
    })
}

/// Run an evaluation suite against this workspace.
fn run_eval(
    invocation: &Invocation,
    suite: &Path,
    trials: Option<u32>,
    strict: bool,
    out: Option<&Path>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let workspace = workspace_root(&invocation.workspace)?;
    let report = eval::run(&workspace, suite, trials, strict, out)?;
    emitter.result(serde_json::to_value(report).map_err(storage_failed)?);
    Ok(0)
}

/// Serving ARSY to something else, and running it unattended.
fn execute_serve(
    invocation: &Invocation,
    emitter: &mut Emitter,
) -> Option<Result<i32, Diagnostic>> {
    Some(match &invocation.command {
        Command::Eval {
            suite,
            trials,
            strict,
            out,
        } => run_eval(invocation, suite, *trials, *strict, out.as_deref(), emitter),
        Command::Serve => serve::run(invocation, emitter),
        Command::ServeAcp => acp::run(invocation, emitter),
        _ => return None,
    })
}

/// Running a task, and everything none of the other groups owns.
fn execute_core(
    invocation: &Invocation,
    tty: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    if let Some(result) = execute_inspect(invocation, emitter) {
        return result;
    }
    if let Some(result) = execute_serve(invocation, emitter) {
        return result;
    }
    match &invocation.command {
        Command::Help => {
            let _ = write!(io::stdout(), "{USAGE}");
            Ok(0)
        }
        Command::Version => {
            let _ = writeln!(io::stdout(), "arsy {} ({})", arsy_code::VERSION, platform());
            Ok(0)
        }
        Command::Tui if !tty => Err(Diagnostic::error(
            "ARSY-SCH-1003",
            "the interactive TUI requires a terminal",
            "there is no terminal; use `arsy run <TASK>`",
        )),
        Command::Tui if emitter.output != Output::Human => Err(usage(
            "the TUI requires human output; use an explicit command with --output json or ci",
        )),
        #[cfg(feature = "tui")]
        Command::Tui => run_tui(invocation, emitter),
        #[cfg(not(feature = "tui"))]
        Command::Tui => Err(Diagnostic::error(
            "ARSY-SCH-1003",
            "the interactive TUI is disabled in this build",
            "install a build with the `tui` feature",
        )),
        Command::Run { task, image } => run(invocation, task, image.as_deref(), emitter),
        Command::Resume { session, follow } => resume(invocation, *session, *follow, emitter),
        Command::Update { check_only } => execute_update(*check_only, emitter),
        Command::Gc {
            apply,
            retention_ms,
        } => evidence::collect(invocation, *apply, *retention_ms, emitter),
        Command::Migrate { apply, backup } => {
            session::migrate(invocation, *apply, backup.as_deref(), emitter)
        }
        // Every other command was answered by one of the groups above; the
        // dispatch chain only reaches here when none of them owned it.
        other => Err(Diagnostic::error(
            "ARSY-SCH-1000",
            format!("{other:?} is not dispatched"),
            "this is a bug in ARSY; report it with the command you ran",
        )),
    }
}

fn execute_update(check_only: bool, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let current = env!("CARGO_PKG_VERSION");
    let report = json!({
        "current_version": current,
        "latest_version": current,
        "up_to_date": true,
        "check_only": check_only,
        "message": format!("arsy-code v{current} is up to date."),
    });
    emitter.result(report);
    Ok(0)
}

/// A redactor that knows every non-interactive credential this workspace has
/// stored, installed on the emitter so anything it prints goes through the
/// same pipeline.
fn redactor(invocation: &Invocation, emitter: &mut Emitter) -> Result<Redactor, Diagnostic> {
    let mut broker = SecretBroker::new();
    broker.register_store(Box::new(OsCredentialStore));
    broker.register_store(Box::new(FileCredentialStore));
    let interactive_tui = matches!(&invocation.command, Command::Tui);
    for record in catalog(CatalogStore::resolve(invocation))? {
        // Opening every OS handle just to prepare a TUI task triggers a
        // keychain prompt before the selected provider or Codex CLI is used.
        // The interactive provider owns its selected credential; the fallback
        // Codex CLI owns its login. File credentials remain safe to preload.
        if interactive_tui && record.handle.store() == OS_STORE_ID {
            continue;
        }
        // A handle that will not open — a revoked entry, a record left behind
        // by a provider since removed — has no value that could reach output,
        // so there is nothing for the redactor to miss.
        let _ = broker.resolve(&record.handle);
    }
    emitter.install_redactor(broker.redactor().clone());
    Ok(broker.redactor().clone())
}

fn compat_explain(
    invocation: &Invocation,
    ecosystem: arsy_code::compat::Ecosystem,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let current = std::env::current_dir().map_err(storage_failed)?;
    let working = if current.starts_with(&root) {
        current
    } else {
        root.clone()
    };
    let fixture = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let report = arsy_code::compat::CompatibilityImporter::new(&root)
        .import(ecosystem, &working, fixture)
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-CMP-1001",
                format!("compatibility import failed: {error}"),
                "fix the reported source or use a supported equal-or-stronger policy mapping",
            )
        })?;
    emitter.result(report.explain());
    Ok(0)
}

fn config_explain(
    invocation: &Invocation,
    key: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let report = load_config(&root, &working, invocation.config.as_deref())?.explain(key);
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        human_config(&report, key)
    });
    Ok(0)
}

/// `config explain` for a reader: one row per key with the value it resolved to
/// and the layer that decided it.
///
/// The machine record carries the full path of every source file; a row names
/// the layer instead and lists the paths once underneath, because the same file
/// otherwise repeats on every line and pushes the values off the screen.
fn human_config(report: &Value, key: Option<&str>) -> Value {
    let Some(values) = report["values"].as_object() else {
        return report.clone();
    };
    if values.is_empty() {
        return json!({"configuration": match key {
            Some(key) => format!("No configuration sets `{key}`."),
            None => "No configuration is set; every value is a built-in default.".to_owned(),
        }});
    }

    let mut listing = format!(
        "{} value{} set\n",
        values.len(),
        if values.len() == 1 { "" } else { "s" }
    );

    let mut default_provider = None;
    let mut endpoints: std::collections::BTreeMap<String, Vec<(String, String, String)>> =
        std::collections::BTreeMap::new();
    let mut others: Vec<(String, String, String)> = Vec::new();
    let mut paths: Vec<&str> = Vec::new();

    for (name, entry) in values {
        let value = entry["value"]
            .as_str()
            .map_or_else(|| plain(&entry["value"]), terminal_text);
        let layer = entry["layer"].as_str().unwrap_or("?");
        if let Some(path) = entry["path"].as_str() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }

        if name == "provider.default" {
            default_provider = Some((value, layer.to_owned()));
        } else if let Some(rest) = name.strip_prefix("provider.endpoint.") {
            if let Some((endpoint_id, field)) = rest.split_once('.') {
                endpoints.entry(endpoint_id.to_owned()).or_default().push((
                    field.to_owned(),
                    value,
                    layer.to_owned(),
                ));
            } else {
                others.push((name.clone(), value, layer.to_owned()));
            }
        } else {
            others.push((name.clone(), value, layer.to_owned()));
        }
    }

    if let Some((val, layer)) = default_provider {
        listing.push_str(&format!("\n  • default provider: {val}  [{layer}]\n"));
    }

    for (endpoint_id, fields) in endpoints {
        listing.push_str(&format!("\n  [{endpoint_id}]\n"));
        let label_width = fields
            .iter()
            .map(|(f, _, _)| f.chars().count())
            .max()
            .unwrap_or(0);
        for (field, val, layer) in fields {
            let pad = " ".repeat(label_width.saturating_sub(field.chars().count()));
            listing.push_str(&format!("    • {field}:{pad}  {val}  [{layer}]\n"));
        }
    }

    if !others.is_empty() {
        listing.push_str("\n  [other]\n");
        let label_width = others
            .iter()
            .map(|(k, _, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        for (name, val, layer) in others {
            let pad = " ".repeat(label_width.saturating_sub(name.chars().count()));
            listing.push_str(&format!("    • {name}:{pad}  {val}  [{layer}]\n"));
        }
    }

    listing.push('\n');
    for path in paths {
        listing.push_str(&format!("  from {}\n", terminal_text(path)));
    }
    json!({"configuration": listing})
}

/// Replace a file's contents in one step.
///
/// A `write` truncates before it writes, so anything reading the file in
/// between sees it empty — and an empty `arsy.json` is a fatal parse error
/// rather than a missing layer. Staging beside the destination and renaming
/// over it means a reader sees either the old contents or the new ones.
fn replace_file(path: &Path, body: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("a settings file needs a directory"))?;
    std::fs::create_dir_all(parent)?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("settings"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(body)?;
        file.sync_all()
    })();
    if written.is_ok() {
        // The staged file is new, so it carries the umask rather than whatever
        // the destination was set to. An operator who tightened a settings
        // file must not have that undone by the next write that touches it.
        #[cfg(unix)]
        if let Ok(existing) = std::fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            let mode = existing.permissions().mode();
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode));
        }
        // Rename replaces on every target ARSY ships for, so nothing is left
        // half written even when the destination is already there.
        if let Err(error) = std::fs::rename(&staged, path) {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(());
    }
    let _ = std::fs::remove_file(&staged);
    written
}

/// Create `~/.arsy/arsy.json` when it is not there yet, carrying over the
/// `config.toml` an older ARSY kept in the platform configuration directory.
///
/// Run before every load rather than only at install time, because ARSY also
/// arrives through Homebrew, Scoop, and npm, and a person who deleted the file
/// should get a working one back rather than a diagnostic.
///
/// Every failure here is silent: a home directory that cannot be written is a
/// run without a user layer, which is exactly what it was before this existed.
/// Nothing is ever overwritten.
fn bootstrap_user_config() {
    let Some(path) = arsy_kernel::config::user_config() else {
        return;
    };
    if path.exists() {
        return;
    }
    // What an older ARSY had, converted once. A file that no longer parses is
    // left where it is: reporting nothing beats replacing settings with an
    // empty file the operator did not ask for.
    //
    // A run pointed at a throwaway configuration home — a test, a container, a
    // second account — asked for that home and not for the operator's own
    // settings copied into it, exactly as the credential catalog treats it.
    let carried = (!config_home_overridden())
        .then(arsy_kernel::config::legacy_user_config)
        .flatten()
        .and_then(|legacy| std::fs::read_to_string(legacy).ok())
        .and_then(|raw| arsy_kernel::config::json_from_toml(&raw, &path).ok());
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = carried.unwrap_or_else(|| "{}".to_owned());
    // Staged beside the destination and linked into place, so a second ARSY
    // starting at the same time reads either nothing or the whole file. A
    // created-then-written file is visible while it is still empty, and an
    // empty `arsy.json` is a fatal parse error rather than a missing layer.
    //
    // `hard_link` rather than the rename `replace_file` uses: it fails when
    // the destination exists, so neither process truncates what the other
    // carried over.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = parent.join(format!(
        ".{}.{}.{}.tmp",
        arsy_kernel::config::CONFIG_FILE,
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let Ok(mut file) = std::fs::File::create(&staged) else {
        return;
    };
    let written = file
        .write_all(body.trim_end().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_ok() {
        let _ = std::fs::hard_link(&staged, &path);
    }
    let _ = std::fs::remove_file(&staged);
}

/// Read every configuration layer for this workspace.
///
/// An invalid file is fatal rather than skipped: continuing with a partly
/// applied policy would silently run under something the operator never wrote.
/// Every discovered configuration layer, plus the one `--config` named.
///
/// The extra file is applied last, so it wins a conflicting value — and only
/// that: `provider.allowed`, `model.allowed`, and the policy rules all merge
/// by intersection, so a session file can narrow the run but never widen it.
fn load_config(
    workspace: &Path,
    working: &Path,
    extra: Option<&Path>,
) -> Result<arsy_kernel::config::Config, Diagnostic> {
    bootstrap_user_config();
    let mut layers = arsy_kernel::config::layers(workspace, working);
    if let Some(path) = extra {
        // Unlike a discovered layer, a path the operator typed is theirs to
        // get right: a missing one is a mistake, not an absent optional file.
        if !path.exists() {
            return Err(Diagnostic::error(
                ARSY_CFG_1000,
                format!("--config names `{}`, which does not exist", path.display()),
                "pass the path to an existing arsy.json, or drop --config",
            ));
        }
        layers.push((arsy_kernel::config::Layer::Session, path.to_path_buf()));
    }
    let unusable = |error: arsy_kernel::config::ConfigError| {
        Diagnostic::error(
            ARSY_CFG_1000,
            format!("configuration is unusable: {error}"),
            "fix the reported file, then run `arsy config explain`",
        )
    };
    // Read twice: the first pass says which tools are switched on and whether
    // this checkout is trusted, and the second places what those tools declare
    // below every layer.
    let config = arsy_kernel::config::Config::load(&layers).map_err(unusable)?;
    let seeds = compat_seeds(workspace, &config);
    let contributes = |seed: &arsy_kernel::config::CompatSeed| {
        !(seed.mcp_servers.is_empty()
            && seed.policy_rules.is_empty()
            && seed.models.is_empty()
            && seed.notes.is_empty())
    };
    if !seeds.iter().any(contributes) {
        return Ok(config);
    }
    arsy_kernel::config::Config::load_with(&layers, &seeds).map_err(unusable)
}

/// What Claude Code and Codex declare for this workspace, read live.
fn compat_seeds(
    workspace: &Path,
    config: &arsy_kernel::config::Config,
) -> Vec<arsy_kernel::config::CompatSeed> {
    let homes = compat_homes();
    arsy_compat::seeds(&arsy_compat::Context {
        homes: &homes,
        root: workspace,
        trusted: config.trusts(workspace),
        claude: config.compat_enabled("claude"),
        codex: config.compat_enabled("codex"),
        env: &|name| std::env::var(name).ok(),
    })
}

/// The model a turn asks for: `--model` when it was given, otherwise whatever
/// configuration resolved.
///
/// A named model is checked against `model.allowed` before it is used. The
/// ceiling is the point of the flag being an override and not an escape: an
/// operator may choose between the models policy permits, and naming one it
/// does not is refused rather than silently ignored or silently obeyed.
fn selected_model(
    config: &Config,
    endpoint: &arsy_kernel::config::Endpoint,
    requested: Option<&str>,
) -> Result<String, Diagnostic> {
    if let Some(model) = requested {
        if !config.model_is_allowed(model) {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("--model `{model}` is excluded by the model.allowed ceiling"),
                "run `arsy model list` for the models this configuration permits",
            ));
        }
        return Ok(model.to_owned());
    }
    endpoint
        .model
        .clone()
        .or_else(|| config.model_default().map(str::to_owned))
        // What Claude Code or Codex is set to use, only when arsy.json is silent.
        .or_else(|| config.compat_model(endpoint).map(str::to_owned))
        .ok_or_else(|| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!("provider `{}` does not say which model to use", endpoint.id),
                "set `model` on the provider endpoint, or `model.default`, in arsy.json, or \
                 pass --model",
            )
        })
}

const CATALOG_NAME: &str = "__catalog__";
/// The catalog under the `file` store, beside the user configuration.
const CATALOG_FILE: &str = "credentials.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthRecord {
    provider: String,
    handle: SecretHandle,
    created_at: u64,
    last_used: Option<u64>,
    /// How the credential was obtained. Defaulted so a catalog written before
    /// OAuth existed still reads.
    #[serde(default)]
    kind: CredentialKind,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialKind {
    #[default]
    ApiKey,
    // Spelled out, because the derived snake_case of `OAuth` is `o_auth`,
    // which is not what an operator reading the catalog expects to see. The
    // alias keeps a catalog written under the derived name readable, so the
    // rename cannot turn one into "corrupt".
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth,
}

/// Where the credential catalog is kept, and how to reach it.
///
/// The catalog is metadata — handles, provider names, timestamps — and never a
/// secret value, so keeping it in the platform store costs an unlock prompt for
/// data that did not need one. `file` is the default for that reason; `os`
/// stays available for an operator who wants everything in one place, chosen
/// with `credentials.store`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogStore {
    File,
    Os,
}

impl CatalogStore {
    /// The configured store, or the default when configuration cannot be read:
    /// listing credentials must not depend on a config file being valid.
    fn resolve(invocation: &Invocation) -> Self {
        workspace_root(&invocation.workspace)
            .ok()
            .and_then(|root| {
                let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
                load_config(&root, &working, invocation.config.as_deref()).ok()
            })
            .map_or(Self::File, |config| Self::named(config.credential_store()))
    }

    fn named(store: &str) -> Self {
        if store == OS_STORE_ID {
            Self::Os
        } else {
            Self::File
        }
    }

    fn read(self) -> Result<Option<String>, Diagnostic> {
        let resolved = match self {
            Self::File => FileCredentialStore.resolve(CATALOG_FILE),
            Self::Os => OsCredentialStore.resolve(CATALOG_NAME),
        };
        match resolved {
            Ok(raw) => Ok(Some(raw)),
            Err(SecretError::NotFound(_)) => Ok(None),
            Err(error) => Err(secret_failed(error)),
        }
    }

    fn write(self, raw: &str) -> Result<(), Diagnostic> {
        match self {
            Self::File => {
                let path = FileCredentialStore::path(CATALOG_FILE).ok_or_else(|| {
                    secret_failed("this platform has no user configuration directory")
                })?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(storage_failed)?;
                }
                // Created owner-only rather than created and then narrowed: a
                // chmod after the write leaves a window where the catalog is
                // readable by the whole machine.
                let mut file = owner_only(&path)?;
                file.write_all(format!("{raw}\n").as_bytes())
                    .map_err(storage_failed)
            }
            Self::Os => OsCredentialStore
                .set(CATALOG_NAME, raw)
                .map_err(secret_failed),
        }
    }
}

/// A catalog file is not a secret, but it names every provider the operator
/// has a credential for, so it is not the whole machine's business either.
/// Truncate or create `path` readable by its owner alone.
///
/// The catalog is not a secret, but it names every provider the operator holds
/// a credential for, which is not the whole machine's business either.
fn owner_only(path: &Path) -> Result<std::fs::File, Diagnostic> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(storage_failed)
}

fn config_home_overridden() -> bool {
    std::env::var_os(arsy_kernel::config::CONFIG_HOME_VAR).is_some_and(|home| !home.is_empty())
}

fn catalog(store: CatalogStore) -> Result<Vec<AuthRecord>, Diagnostic> {
    let raw = match store.read()? {
        Some(raw) => Some(raw),
        // Nothing here yet, so take what the other store already had. This is
        // what moves an existing catalog across once, and it reads the platform
        // store exactly once rather than on every turn.
        // A run pointed at a throwaway configuration home — a test, a
        // container, a second account — asked for that home and not for the
        // operator's own credentials copied into it.
        None if store == CatalogStore::File && !config_home_overridden() => {
            // A platform store that is unavailable, or whose prompt was
            // declined, means there is nothing to migrate — not that every
            // later turn should fail on a convenience.
            let migrated = CatalogStore::Os.read().unwrap_or_default();
            if let Some(raw) = &migrated {
                store.write(raw)?;
            }
            migrated
        }
        None => None,
    };
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|_| secret_failed("credential catalog is corrupt"))
}

fn save_catalog(store: CatalogStore, records: &[AuthRecord]) -> Result<(), Diagnostic> {
    let raw = serde_json::to_string(records).map_err(|error| secret_failed(error.to_string()))?;
    store.write(&raw)
}

fn auth_set(
    invocation: &Invocation,
    provider: &str,
    requested: Option<&str>,
    tty: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let name = requested.unwrap_or(provider);
    let handle = SecretHandle::new(OS_STORE_ID, name).map_err(secret_failed)?;
    let mut secret = if tty {
        rpassword::prompt_password(format!("Credential for {provider}: ")).map_err(secret_failed)?
    } else {
        read_stdin()?
    };
    while secret.ends_with(['\n', '\r']) {
        secret.pop();
    }
    if secret.len() < arsy_kernel::secret::MIN_SECRET_BYTES {
        return Err(secret_failed("credential is too short to redact safely"));
    }
    // The value belongs in the platform store; the catalog goes wherever the
    // operator configured, which is not the same question.
    let store = OsCredentialStore;
    let records_store = CatalogStore::resolve(invocation);
    let previous = match store.resolve(name) {
        Ok(value) => Some(value),
        Err(SecretError::NotFound(_)) => None,
        Err(error) => return Err(secret_failed(error)),
    };
    store.set(name, &secret).map_err(secret_failed)?;
    let mut records = catalog(records_store)?;
    let now = now()?;
    if let Some(record) = records.iter_mut().find(|record| record.handle == handle) {
        record.provider = provider.to_owned();
        record.kind = CredentialKind::ApiKey;
    } else {
        records.push(AuthRecord {
            provider: provider.to_owned(),
            handle: handle.clone(),
            created_at: now,
            last_used: None,
            kind: CredentialKind::ApiKey,
        });
    }
    if let Err(error) = save_catalog(records_store, &records) {
        if let Some(previous) = previous {
            let _ = store.set(name, &previous);
        } else {
            let _ = store.remove(name);
        }
        return Err(error);
    }
    emitter.result(json!({"provider": provider, "handle": handle}));
    Ok(0)
}

/// The OAuth client and endpoint context a login runs against, resolved once
/// so both the exchange and the credential-storage step that follows it
/// share the same view.
struct OAuthLogin {
    config: Config,
    configured: Option<arsy_kernel::config::Endpoint>,
    preset: Option<&'static arsy_kernel::oauth::presets::Preset>,
    oauth: arsy_kernel::config::OAuth,
    synthesize: bool,
}

/// Resolve which OAuth client `arsy auth login <provider>` should run: a
/// hand-configured endpoint's own `[oauth]` table, or a built-in preset
/// standing in for one.
///
/// Split from the exchange itself so a caller that cannot finish the
/// exchange in the same call — the TUI, waiting for the operator to paste a
/// code back on a later turn — can resolve the same client twice, once to
/// start the login and again once the pasted code comes back, and get the
/// same answer both times.
fn resolve_oauth_login(invocation: &Invocation, provider: &str) -> Result<OAuthLogin, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working, invocation.config.as_deref())?;
    let configured = config.endpoint(Some(provider)).cloned();
    let preset = arsy_kernel::oauth::presets::get(provider);

    // The OAuth client to run the flow with. A built-in preset stands in when
    // the endpoint names none, and when the endpoint does not exist at all its
    // `[provider.endpoint]` table is written after the token is stored.
    let (oauth, synthesize) = match (&configured, preset) {
        (Some(endpoint), _) if endpoint.oauth.is_some() => {
            (endpoint.oauth.clone().expect("checked"), false)
        }
        (Some(_), Some(preset)) => (preset.oauth(), false),
        (Some(_), None) => {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("provider `{provider}` has no OAuth client configured"),
                format!(
                    "add a `[provider.endpoint.{provider}.oauth]` table, or store an API key \
                     with `arsy auth set {provider}`"
                ),
            ))
        }
        (None, Some(preset)) => (preset.oauth(), true),
        (None, None) => {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("no provider endpoint named `{provider}` is configured"),
                format!(
                    "configure `[provider.endpoint.{provider}]`, or sign in to a built-in \
                     preset: {}",
                    preset_ids()
                ),
            ))
        }
    };
    Ok(OAuthLogin {
        config,
        configured,
        preset,
        oauth,
        synthesize,
    })
}

/// Store the token set a login produced, under the same kind of handle an
/// API key would use, so everything downstream — resolution, redaction,
/// `auth remove` — treats the two the same.
fn store_oauth_login(
    invocation: &Invocation,
    provider: &str,
    login: &OAuthLogin,
    tokens: arsy_kernel::oauth::TokenSet,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let (store_kind, handle_name) = match login
        .configured
        .as_ref()
        .and_then(|e| e.credential.as_ref())
    {
        Some(existing) => (existing.store(), existing.name().to_owned()),
        None => {
            if login.config.credential_store() == FILE_STORE_ID {
                (FILE_STORE_ID, format!("{provider}.key"))
            } else {
                (OS_STORE_ID, provider.to_owned())
            }
        }
    };
    let handle = SecretHandle::new(store_kind, &handle_name).map_err(secret_failed)?;
    let raw = serde_json::to_string(&tokens).map_err(|error| secret_failed(error.to_string()))?;
    if store_kind == FILE_STORE_ID {
        FileCredentialStore
            .set(&handle_name, &raw)
            .map_err(secret_failed)?;
    } else {
        OsCredentialStore
            .set(handle.name(), &raw)
            .map_err(secret_failed)?;
    }
    let records_store = CatalogStore::resolve(invocation);
    let mut records = catalog(records_store)?;
    let now = now()?;
    match records.iter_mut().find(|record| record.handle == handle) {
        Some(record) => {
            record.provider = provider.to_owned();
            record.kind = CredentialKind::OAuth;
        }
        None => records.push(AuthRecord {
            provider: provider.to_owned(),
            handle: handle.clone(),
            created_at: now,
            last_used: None,
            kind: CredentialKind::OAuth,
        }),
    }
    save_catalog(records_store, &records)?;

    // A preset that had no endpoint of its own gets one written now, pointed at
    // the credential just stored, so `/model` and a turn find it like any other.
    let mut wrote_endpoint = false;
    if login.synthesize {
        let preset = login
            .preset
            .expect("synthesize is only set when a preset matched");
        let endpoint = config_edit::Endpoint {
            name: provider.to_owned(),
            kind: preset.dialect.as_str().to_owned(),
            base_url: preset.base_url.to_owned(),
            models: preset
                .models
                .iter()
                .map(|model| (*model).to_owned())
                .collect(),
            credential: handle.to_string(),
        };
        write_config(|config| config_edit::append_endpoint(config, &endpoint)).map_err(
            |error| {
                Diagnostic::error(
                    ARSY_PRV_1000,
                    format!(
                    "signed in, but `provider.endpoint.{provider}` could not be written: {error}"
                ),
                    "add the endpoint by hand; the credential is already stored",
                )
            },
        )?;
        wrote_endpoint = true;
    }

    emitter.result(json!({
        "provider": provider,
        "handle": handle,
        "kind": "oauth",
        "expires_at": tokens.expires_at,
        "endpoint_written": wrote_endpoint,
    }));
    Ok(0)
}

/// Sign in to a provider through the OAuth client its configuration names.
fn auth_login(
    invocation: &Invocation,
    provider: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let login = resolve_oauth_login(invocation, provider)?;
    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let tokens = if arsy_kernel::oauth::uses_device_grant(&login.oauth) {
        let prompt =
            arsy_kernel::oauth::begin_device(&transport, &login.oauth).map_err(login_failed)?;
        // Printed rather than opened: the operator may be on another machine,
        // and this is the grant that does not need a local browser at all.
        emitter.result(json!({
            "provider": provider,
            "verification_uri": prompt.verification_uri_complete
                .clone()
                .unwrap_or_else(|| prompt.verification_uri.clone()),
            "user_code": prompt.user_code,
        }));
        arsy_kernel::oauth::poll_device(&transport, &login.oauth, &prompt, &mut std::thread::sleep)
            .map_err(login_failed)?
    } else if arsy_kernel::oauth::uses_manual_grant(&login.oauth) {
        // Anthropic's Claude Code OAuth client has no loopback redirect: the
        // issuer's own hosted page shows the operator a code to paste back.
        let prompt = arsy_kernel::oauth::begin_manual(&login.oauth).map_err(login_failed)?;
        let interactive = emitter.output == Output::Human;
        let opened = interactive && open_browser(&prompt.authorize_url);
        let _ = writeln!(
            io::stderr(),
            "{}\n  {}\nAfter you approve, paste the code shown back here:",
            if opened {
                "Opening your browser to sign in. If it did not open, visit:"
            } else {
                "Open this URL to sign in:"
            },
            prompt.authorize_url
        );
        let pasted = read_pasted_code().map_err(login_failed)?;
        arsy_kernel::oauth::finish_manual(&transport, &login.oauth, &prompt.verifier, &pasted)
            .map_err(login_failed)?
    } else {
        // Open the browser for an interactive operator; a scripted or headless
        // run (`--output json|ci`) only prints the URL. Either way the URL is
        // printed, so a browser that does not open is not a dead end.
        let interactive = emitter.output == Output::Human;
        let tokens =
            arsy_kernel::oauth::authorization_code(&transport, &login.oauth, &mut |authorize| {
                let opened = interactive && open_browser(authorize);
                let _ = writeln!(
                    io::stderr(),
                    "{}\n  {authorize}",
                    if opened {
                        "Opening your browser to sign in. If it did not open, visit:"
                    } else {
                        "Open this URL to sign in:"
                    }
                );
            });
        tokens.map_err(login_failed)?
    };
    store_oauth_login(invocation, provider, &login, tokens, emitter)
}

/// The built-in preset ids, for an error that offers them as an alternative.
fn preset_ids() -> String {
    arsy_kernel::oauth::presets::all()
        .iter()
        .map(|preset| preset.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hand the URL to the platform's browser opener. Best-effort: the return
/// says the opener was launched, not that a browser appeared.
fn open_browser(url: &str) -> bool {
    #[cfg(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    ))]
    {
        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = std::process::Command::new("xdg-open");
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", ""]);
            command
        };
        command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    {
        let _ = url;
        false
    }
}

/// Read the one line the operator pastes back for a manual OAuth grant: the
/// code an issuer's hosted callback page showed them, once they approved.
/// Unlike [`read_stdin`], this reads a single line rather than to EOF — the
/// operator presses Enter, not Ctrl-D.
fn read_pasted_code() -> Result<String, arsy_kernel::oauth::OAuthError> {
    let _ = write!(io::stderr(), "Paste the code here: ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| arsy_kernel::oauth::OAuthError::Local(error.to_string()))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(arsy_kernel::oauth::OAuthError::Abandoned(
            "no code was pasted".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn login_failed(error: arsy_kernel::oauth::OAuthError) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        error.to_string(),
        "check the OAuth client in `arsy config explain`, then run `arsy auth login` again",
    )
}

fn auth_list(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let records = catalog(CatalogStore::resolve(invocation))?;
    emitter.result(if emitter.output == Output::Json {
        json!({"credentials": records})
    } else {
        human_credentials(&records)
    });
    Ok(0)
}

/// `auth list` for a reader: one row per credential, handle and kind first,
/// because the handle is what a `credential = ` line has to be pointed at.
///
/// Values are never read here, only handles, so nothing on these rows is a
/// secret.
fn human_credentials(records: &[AuthRecord]) -> Value {
    if records.is_empty() {
        return json!({
            "credentials": "No credentials are stored. `arsy auth set <PROVIDER>` stores one and \
                            prints the handle to point `credential` at."
        });
    }
    let handles: Vec<String> = records
        .iter()
        .map(|record| record.handle.to_string())
        .collect();
    let label = handles
        .iter()
        .map(|handle| handle.chars().count())
        .max()
        .unwrap_or(0);
    let mut listing = format!(
        "{} credential{} stored\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    );
    // `oauth` and `api_key` differ in width, so the column after them only
    // lines up if the kind is padded too.
    let kind = |record: &AuthRecord| plain(&json!(record.kind));
    let kinds = records.iter().map(kind).collect::<Vec<_>>();
    let kind_label = kinds.iter().map(|kind| kind.len()).max().unwrap_or(0);
    for ((record, handle), kind) in records.iter().zip(&handles).zip(&kinds) {
        listing.push_str(&format!(
            "\n  {handle}{}  {kind}{}  provider {}{}",
            " ".repeat(label - handle.chars().count()),
            " ".repeat(kind_label - kind.len()),
            terminal_text(&record.provider),
            if record.last_used.is_some() {
                ""
            } else {
                "  · never used"
            },
        ));
    }
    listing.push('\n');
    json!({"credentials": listing})
}

fn auth_remove(
    invocation: &Invocation,
    handle: &SecretHandle,
    _force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    // Both stores can be removed from, because both can be listed: a catalog
    // that names a handle no command can delete is a catalog that only grows.
    if !matches!(handle.store(), OS_STORE_ID | FILE_STORE_ID) {
        return Err(secret_failed(format!(
            "no credential store `{}` to remove from",
            handle.store()
        )));
    }
    let records_store = CatalogStore::resolve(invocation);
    let original = catalog(records_store)?;
    let mut records = original.clone();
    records.retain(|record| &record.handle != handle);
    save_catalog(records_store, &records)?;
    let removed = match handle.store() {
        FILE_STORE_ID => FileCredentialStore.remove(handle.name()),
        _ => OsCredentialStore.remove(handle.name()),
    };
    if let Err(error) = removed {
        // The catalog is written first, so a failed delete has to put it back
        // rather than leave a stored credential nothing lists.
        let _ = save_catalog(records_store, &original);
        return Err(secret_failed(error));
    }
    emitter.result(json!({"removed": handle, "referenced_by": []}));
    Ok(0)
}

fn now() -> Result<u64, Diagnostic> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(secret_failed)
}

fn secret_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-CRD-1000",
        format!("credential operation failed: {}", error.to_string()),
        "unlock or configure the OS credential store and retry",
    )
}

#[cfg(feature = "tui")]
fn terminal_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-UIX-1000",
        format!("interactive terminal failed: {}", error.to_string()),
        "check the terminal input and output, then retry",
    )
}

/// What the composer is currently collecting a line for.
#[cfg(feature = "tui")]
enum Prompt {
    Task,
    Model,
    Effort,
    Theme,
    Provider(tui::ProviderStep),
    Auth(tui::AuthStep),
    Resume,
    Session(tui::SessionDialogState),
}

#[cfg(feature = "tui")]
const IMPLEMENT_APPROVED_PLAN: &str =
    "Implement the approved plan now. Reuse the findings and constraints already established in this conversation, update the existing plan steps as work progresses, and run the planned validation.";

/// What `/plan` was asked to do.
///
/// Parsed away from the terminal loop so the lifecycle — enter, revise,
/// approve, cancel — can be tested without a session, a provider, or a tty.
#[cfg(feature = "tui")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum PlanCommand {
    /// Enter Plan Mode, optionally with the task to plan.
    Enter(Option<String>),
    /// Stay in Plan Mode and plan again, optionally with operator feedback.
    Revise(Option<String>),
    Approve,
    Cancel,
    /// Print the plan the model is working to, without changing anything.
    Show,
}

/// The subcommand is the whole first word or it is not the subcommand: a
/// `/plan approve when you can` that fell through to `Enter` would re-enter
/// planning and queue the operator's sentence as the thing to plan.
#[cfg(feature = "tui")]
fn plan_command(line: &str) -> PlanCommand {
    let argument = line.trim().trim_start_matches("/plan").trim();
    let (head, rest) = argument
        .split_once(char::is_whitespace)
        .unwrap_or((argument, ""));
    let note = |text: &str| {
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    };
    match head {
        "revise" => PlanCommand::Revise(note(rest)),
        "approve" => PlanCommand::Approve,
        "cancel" => PlanCommand::Cancel,
        "show" | "list" => PlanCommand::Show,
        _ => PlanCommand::Enter(note(argument)),
    }
}

/// The turn a revision asks for. One wording, whether the operator typed
/// `/plan revise` or chose it in the plan dialog.
#[cfg(feature = "tui")]
fn revise_instruction(note: Option<&str>) -> String {
    match note {
        Some(note) => {
            format!("Revise the plan using this operator feedback, without making changes: {note}")
        }
        None => "Continue planning. Reinspect any uncertain paths and return a revised implementation plan without making changes.".to_owned(),
    }
}

#[cfg(feature = "tui")]
fn set_approval_mode(
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    mode: approval::ApprovalMode,
) {
    approval.set(mode);
    state.set_approval_mode(mode.label());
}

#[cfg(feature = "tui")]
fn cycle_approval_mode(approval: &approval::ApprovalCell) -> approval::ApprovalMode {
    let mode = approval.get().cycle();
    approval.set(mode);
    mode
}

#[cfg(feature = "tui")]
fn run_tui(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let workspace = workspace_root(&invocation.workspace)?;
    let mut stdout = io::stdout();

    let Opened {
        native,
        native_requested,
        detected,
        provider_available,
    } = open_route(invocation, &workspace)?;
    let colour = !invocation.no_color && std::env::var_os("NO_COLOR").is_none();

    let (theme_config, mut theme) = open_palette(invocation, &workspace, emitter);
    let mut models = {
        let mut models = endpoint_models(invocation);
        models.extend(tui::available_models());
        models
    };
    let remembered = saved_route().filter(|saved| saved.provider == detected.provider);
    let mut route = remembered.clone().unwrap_or(detected);
    let (mut resolved_providers, mut unavailable_providers) =
        seed_providers(native, native_requested.as_deref(), &route.provider);
    let mut effort = saved_effort();
    // What `/provider` is holding between its questions, and the list it offers.
    let mut draft = tui::ProviderDraft::default();
    let mut providers = configured_providers(invocation);
    // What the configuration names now, which is not what this session resolved
    // once `/provider` has switched and the restart has not happened yet.
    let mut chosen_provider = configured_default(invocation);
    let mut conversation: Vec<ModelMessage> = Vec::new();
    let mut transcript = tui::Transcript::default();
    // The events the restored prefix came from, so a compaction of it can cite
    // something a reader can still open.
    let mut history = arsy_code::agent::budget::History::default();
    let mut auth_draft = String::new();
    let mut sessions: Vec<tui::SessionChoice> = Vec::new();
    let approval =
        std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
    // MCP servers start connecting now, in the background, so the first turn
    // finds them up or on their way instead of starting them itself.
    let working = std::env::current_dir().unwrap_or_else(|_| workspace.to_path_buf());
    if let Ok(config) = load_config(&workspace, &working, invocation.config.as_deref()) {
        session_connector().sync(&config);
    }

    let mut state = tui::TuiState::new(workspace.display().to_string(), SessionId::new());
    state.set_effort(effort);
    state.set_model_route(route.clone());
    state.set_approval_mode(approval.get().label());
    // The first drawing of the card, so the loop below does not read it as a
    // change and repaint over the notices printed under it.
    state.card_is_stale();
    writeln!(stdout, "{}", state.render(tui::terminal_width(), colour)).map_err(terminal_failed)?;
    writeln!(
        stdout,
        "Use /help for commands, /mcp and /hooks to inspect integrations."
    )
    .map_err(terminal_failed)?;
    if !provider_available {
        writeln!(stdout, "Provider unavailable. Inspection is available; configure a `[provider.endpoint.<name>]` table and run `arsy auth set <name>`, or install Codex and run codex login, to execute tasks.").map_err(terminal_failed)?;
    }

    // ARSY paints the input line from here on, so it owns the terminal modes
    // and is the only reader of stdin.
    let _raw = tui::RawTerminal::acquire().map_err(terminal_failed)?;
    let keys = tui::spawn_key_reader();
    let mut decoder = tui::Keys::default();
    let mut composer = tui::Composer::default();

    // A remembered model skips the picker; `/model` reopens it.
    // A line submitted while a turn was running runs next, before stdin is
    // read again.
    let mut queued = std::collections::VecDeque::new();
    let mut prompt = if remembered.is_some() || !route.model.is_empty() {
        Prompt::Task
    } else {
        Prompt::Model
    };

    loop {
        // `/model` changes what the launch card says, and the card is the
        // first thing a reader checks. The approval mode lives in the status
        // row, so Shift+Tab never lands here. A fresh card
        // is printed rather than the screen being repainted around the old
        // one, because a repaint also erases the notices printed between the
        // cards — including the line that just reported the change.
        if state.card_is_stale() {
            write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
            writeln!(stdout, "{}", state.render(tui::terminal_width(), colour))
                .map_err(terminal_failed)?;
            composer.invalidate();
        }
        let status = prompt_status(
            &prompt,
            Picker {
                state: &state,
                workspace: &workspace,
                models: &models,
                route: &mut route,
                effort,
                theme: &theme,
                draft: &draft,
                auth_draft: &auth_draft,
                sessions: &sessions,
                providers: &providers,
                chosen_provider: chosen_provider.as_deref(),
            },
            colour,
        );
        // Derived from the prompt once per line, so the command menu can never
        // drift out of step with which prompt is collecting the answer.
        composer.set_picking(!matches!(prompt, Prompt::Task));
        offer_rows(
            &prompt,
            &mut composer,
            Picker {
                state: &state,
                workspace: &workspace,
                models: &models,
                route: &mut route,
                effort,
                theme: &theme,
                draft: &draft,
                auth_draft: &auth_draft,
                sessions: &sessions,
                providers: &providers,
                chosen_provider: chosen_provider.as_deref(),
            },
            invocation,
        );
        composer.set_masked(masked(&prompt));
        // While the theme picker is open, repaint in whichever theme is
        // arrowed onto so it can be seen before Enter takes it.
        let preview_theme = |name: &str| tui::set_palette(name, &theme_config.roles);
        let preview: Option<&dyn Fn(&str)> = match prompt {
            Prompt::Theme => Some(&preview_theme),
            _ => None,
        };
        let input = match queued.pop_front() {
            Some(line) => tui::Action::Submit(line),
            None => match read_line(ReadLineContext {
                keys: &keys,
                decoder: &mut decoder,
                composer: &mut composer,
                stdout: &mut stdout,
                colour,
                status: &status,
                preview,
                transcript: &mut transcript,
                state: &state,
            })? {
                Some(input) => input,
                // Ending input at a picker cancels the picker, not the
                // session: the setting is unchanged and the task prompt
                // returns. Ending it at the task prompt ends the session.
                None if cancels_to_task(&prompt) => {
                    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                    let unchanged = leave_picker(
                        &prompt,
                        Leaving {
                            effort,
                            theme: &theme,
                            roles: &theme_config.roles,
                            draft: &mut draft,
                            auth_draft: &mut auth_draft,
                            session: state.session_id(),
                            route: &mut route,
                        },
                    );
                    writeln!(stdout, "{unchanged}").map_err(terminal_failed)?;
                    prompt = Prompt::Task;
                    continue;
                }
                None => break,
            },
        };
        // Shift+Tab changes the mode where it stands: it never becomes a line
        // for the prompt to answer. Every other action redraws and nothing
        // more.
        let Some(line) = submitted(input, &approval, &mut state) else {
            continue;
        };
        let pass = answer_prompt(
            prompt,
            &line,
            invocation,
            Typing {
                workspace: &workspace,
                colour,
                provider_available,
                route: &mut route,
                effort: &mut effort,
                models: &mut models,
                providers: &mut providers,
                chosen: &mut chosen_provider,
                draft: &mut draft,
                auth_draft: &mut auth_draft,
                theme: &mut theme,
                roles: &theme_config.roles,
                sessions: &mut sessions,
                resolved_providers: &mut resolved_providers,
                unavailable_providers: &mut unavailable_providers,
            },
            Restoring {
                workspace: &workspace,
                state: &mut state,
                conversation: &mut conversation,
                transcript: &mut transcript,
                history: &mut history,
                approval: &approval,
                queued: &mut queued,
            },
            &mut stdout,
            &keys,
            &mut decoder,
            &mut composer,
            emitter,
        )?;
        prompt = match pass {
            TaskPass::Stop => break,
            TaskPass::Ask(next) => next,
            TaskPass::Go => Prompt::Task,
        };
    }
    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
    stdout.flush().map_err(terminal_failed)?;
    Ok(0)
}

/// Collect input, repainting the composer after every key that changes it.
///
/// `CycleMode` is returned separately from submitted text so Shift+Tab never
/// becomes a task or a history entry. `None` means the session should end.
#[cfg(feature = "tui")]
struct ReadLineContext<'a> {
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    composer: &'a mut tui::Composer,
    stdout: &'a mut dyn Write,
    colour: bool,
    status: &'a str,
    preview: Option<&'a dyn Fn(&str)>,
    transcript: &'a mut tui::Transcript,
    state: &'a tui::TuiState,
}

fn read_line(
    ReadLineContext {
        keys,
        decoder,
        composer,
        stdout,
        colour,
        status,
        preview,
        transcript,
        state,
    }: ReadLineContext<'_>,
) -> Result<Option<tui::Action>, Diagnostic> {
    let mut width = tui::terminal_width();
    composer.set_height(tui::terminal_rows());
    let mut measured = std::time::Instant::now();
    loop {
        let refreshed = std::time::Instant::now();
        if measured.elapsed() >= std::time::Duration::from_millis(100) {
            let next_width = tui::terminal_width();
            if next_width != width {
                transcript
                    .repaint(stdout, next_width, colour, state)
                    .map_err(terminal_failed)?;
                composer.invalidate();
            }
            width = next_width;
            composer.set_height(tui::terminal_rows());
            measured = std::time::Instant::now();
        }
        if let (Some(preview), Some(row)) = (preview, composer.highlighted()) {
            preview(&row);
        }
        write!(stdout, "{}", composer.render(width, colour, status)).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        loop {
            // The timeout is what tells a lone Escape apart from the start of
            // an arrow-key sequence.
            let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
                Ok(byte) => decoder.feed(byte),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let key = decoder.flush_escape();
                    if key.is_none() && refreshed.elapsed() >= std::time::Duration::from_millis(100)
                    {
                        break;
                    }
                    key
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            };
            let Some(key) = key else { continue };
            match composer.press(key) {
                tui::Action::Submit(line) => return Ok(Some(tui::Action::Submit(line))),
                tui::Action::CycleMode => return Ok(Some(tui::Action::CycleMode)),
                tui::Action::Quit => return Ok(None),
                tui::Action::Redraw => break,
                tui::Action::None => {}
            }
        }
    }
}

/// Persist the picked model, reporting only that persistence failed — the
/// choice still applies to this session.
#[cfg(feature = "tui")]
fn remember_model(route: &tui::ModelRoute, emitter: &mut Emitter) {
    if let Err(error) = save_route(route) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the model choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
fn remember_effort(effort: Option<Effort>, emitter: &mut Emitter) {
    if let Err(error) = save_effort(effort) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the effort choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

/// The remembered model lives beside the user configuration layer that
/// `arsy doctor` already reports.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("model"))
}

/// The route chosen last time, as `provider/model`.
///
/// The model is re-validated on read: a file written by an older build that
/// accepted anything must not keep selecting an unusable model on every later
/// start.
#[cfg(feature = "tui")]
fn saved_route() -> Option<tui::ModelRoute> {
    let raw = std::fs::read_to_string(model_store()?).ok()?;
    let raw = raw.trim();
    let route = (!raw.is_empty()).then(|| tui::ModelRoute::parse(raw))?;
    tui::validate_slug(&route.model).ok()?;
    Some(route)
}

#[cfg(feature = "tui")]
fn save_route(route: &tui::ModelRoute) -> io::Result<()> {
    let path = model_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{route}\n").as_bytes())
}

/// The remembered reasoning effort, beside the remembered model.
#[cfg(feature = "tui")]
use arsy_kernel::provider::Effort;

#[cfg(feature = "tui")]
fn effort_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("effort"))
}

/// The effort chosen last time, re-validated on read for the same reason the
/// model is: an unreadable file must not decide what a turn sends.
#[cfg(feature = "tui")]
fn saved_effort() -> Option<Effort> {
    Effort::parse(std::fs::read_to_string(effort_store()?).ok()?.trim())
}

/// `None` clears the choice, so a turn goes back to carrying no reasoning knob.
#[cfg(feature = "tui")]
fn save_effort(effort: Option<Effort>) -> io::Result<()> {
    let path = effort_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    match effort {
        Some(effort) => replace_file(&path, format!("{effort}\n").as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    }
}

/// The remembered colour theme, beside the remembered effort.
#[cfg(feature = "tui")]
fn theme_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("theme"))
}

/// The theme chosen last time, kept only if it is still a built-in name: a
/// file written by a build that knew a theme this one dropped must not select
/// nothing.
#[cfg(feature = "tui")]
fn saved_theme() -> Option<String> {
    let raw = std::fs::read_to_string(theme_store()?).ok()?;
    let name = raw.trim().to_owned();
    tui::builtin_palette(&name).map(|_| name)
}

#[cfg(feature = "tui")]
fn save_theme(name: &str) -> io::Result<()> {
    let path = theme_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{name}\n").as_bytes())
}

#[cfg(feature = "tui")]
fn remember_theme(name: &str, emitter: &mut Emitter) {
    if let Err(error) = save_theme(name) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the theme choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
fn apply_theme(
    answer: &str,
    current: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> io::Result<bool> {
    let picked = match tui::resolve_theme_answer(answer, current) {
        Ok(picked) => picked,
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason))?;
            return Ok(false);
        }
    };
    tui::set_palette(&picked, roles);
    *current = picked;
    remember_theme(current, emitter);
    writeln!(stdout, "Theme: {current}")?;
    Ok(true)
}

#[cfg(feature = "tui")]
fn endpoint_models(invocation: &Invocation) -> Vec<tui::ModelChoice> {
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return Vec::new();
    };
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let mut choices = Vec::new();
    if let Ok(config) = load_config(&root, &working, invocation.config.as_deref()) {
        for endpoint in config.endpoints() {
            choices.extend(endpoint.models.iter().map(|slug| tui::ModelChoice {
                provider: endpoint.id.clone(),
                slug: slug.clone(),
                name: format!("on {}", endpoint.id),
            }));
        }
    }
    // Model discovery must be read-only. Probing the macOS keychain here
    // triggers an unlock prompt every time `/model` opens; auth state is already
    // represented by the credential catalog.
    let saved_handles = catalog_handles(invocation);
    for preset in arsy_kernel::oauth::presets::all() {
        let has_auth = saved_handles.iter().any(|h| h.contains(preset.id));
        if has_auth && !choices.iter().any(|c| c.provider == preset.id) {
            choices.extend(preset.models.iter().map(|slug| tui::ModelChoice {
                provider: preset.id.to_string(),
                slug: (*slug).to_string(),
                name: format!("on {}", preset.id),
            }));
        }
    }
    choices
}

/// The palette the session paints with: a built-in base — the `[theme]` base,
/// else the remembered theme, else the default — with any `[theme]` role
/// overrides on top. Returns the base name (for the `/theme` picker) and the
/// palette, or the reason an override was rejected.
#[cfg(feature = "tui")]
fn resolve_palette(theme: &arsy_kernel::config::Theme) -> (String, Result<tui::Palette, String>) {
    let base = theme
        .base
        .clone()
        .or_else(saved_theme)
        .unwrap_or_else(|| tui::DEFAULT_THEME.to_owned());
    let palette = tui::builtin_palette(&base).unwrap_or_else(|| {
        tui::builtin_palette(tui::DEFAULT_THEME).expect("the default theme is built in")
    });
    let built = if theme.roles.is_empty() {
        Ok(palette)
    } else {
        palette.with_overrides(&theme.roles)
    };
    (base, built)
}

/// Where `/provider` goes after an answer.
#[cfg(feature = "tui")]
enum ProviderNext {
    Ask(tui::ProviderStep),
    Done(String),
    Cancelled(String),
}

/// The provider the configuration names right now.
#[cfg(feature = "tui")]
fn configured_default(invocation: &Invocation) -> Option<String> {
    let root = workspace_root(&invocation.workspace).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    load_config(&root, &working, invocation.config.as_deref())
        .ok()?
        .provider_default()
        .map(str::to_owned)
}

#[cfg(feature = "tui")]
fn load_workspace_sessions(workspace: &Path) -> Vec<tui::SessionChoice> {
    let Ok(store) = open_store(workspace) else {
        return Vec::new();
    };
    let Ok(summaries) = store.sessions(30) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let ts = s.last_event_at_ms.or(s.started_at_ms).unwrap_or_default();
            let last_seen = if ts > 0 {
                let now = arsy_kernel::artifact::unix_time_ms();
                let diff_secs = now.saturating_sub(ts) / 1000;
                if diff_secs < 60 {
                    "just now".to_owned()
                } else if diff_secs < 3600 {
                    format!("{}m ago", diff_secs / 60)
                } else if diff_secs < 86400 {
                    format!("{}h ago", diff_secs / 3600)
                } else {
                    format!("{}d ago", diff_secs / 86400)
                }
            } else {
                "recorded".to_owned()
            };
            tui::SessionChoice {
                id: s.session,
                title: s.title,
                events: s.version.0,
                last_seen,
            }
        })
        .collect()
}

#[cfg(feature = "tui")]
/// The conversation a resumed session continues from.
///
/// Built from completed turns only. A turn that failed or was interrupted
/// wrote no completion, so its prompt is not replayed: a question the model
/// never answered, restored as history, reads as something that happened and
/// is worse than a gap.
///
/// Each completed turn contributes the exchange it recorded — prompt,
/// replies, tool calls, tool results — or, for a stream written before
/// transcripts existed, whatever the two ends of it can be reconstructed from.
fn reconstruct_session_conversation(
    workspace: &Path,
    session: SessionId,
) -> (Vec<ModelMessage>, arsy_code::agent::budget::History) {
    let mut history = arsy_code::agent::budget::History::default();
    let Ok(store) = open_store(workspace) else {
        return (Vec::new(), history);
    };
    let Ok(events) = store.read(session, 1, 1000) else {
        return (Vec::new(), history);
    };
    let inline = |event: &arsy_kernel::event::EventEnvelope| {
        let arsy_kernel::event::EventPayload::Inline { data } = &event.payload else {
            return None;
        };
        Some(data.clone())
    };
    let turn_of = |data: &Value| {
        data.get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    // Two passes, because a transcript is written just before its turn is
    // closed and a turn that never closed must contribute nothing. One pass
    // could not know, at the transcript, whether the completion would come.
    let mut completed = std::collections::HashSet::new();
    let mut prompts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        match event.kind.as_str() {
            "turn.completed" => {
                completed.insert(turn_of(&data));
            }
            "turn.started" => {
                if let Some(prompt) = data.get("prompt").and_then(Value::as_str) {
                    prompts.insert(turn_of(&data), prompt.to_owned());
                }
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    let mut transcribed = std::collections::HashSet::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        let turn = turn_of(&data);
        if !completed.contains(&turn) {
            continue;
        }
        match event.kind.as_str() {
            "turn.transcript" => {
                let recorded = transcript::restore(data.get("transcript").unwrap_or(&Value::Null));
                if !recorded.is_empty() {
                    transcribed.insert(turn);
                    messages.extend(recorded);
                    // What a later compaction of this prefix would cite: the
                    // event that holds the exchange verbatim.
                    history.citations.push(arsy_kernel::context::EventCitation {
                        id: event.id,
                        sequence: event.sequence,
                    });
                }
            }
            // A stream written before transcripts existed, or one whose
            // transcript did not survive. The question it was asked is what it
            // has, and it is better than nothing.
            "turn.completed" if !transcribed.contains(&turn) => {
                if let Some(prompt) = prompts.remove(&turn) {
                    messages.push(ModelMessage {
                        role: ModelRole::User,
                        content: vec![ModelContent::Text { text: prompt }],
                    });
                }
            }
            _ => {}
        }
    }
    (messages, history)
}

/// The providers configured right now, in the order the configuration lists
/// them. Read fresh each time `/provider` opens, so an edit made outside ARSY
/// is not hidden behind a stale list.
#[cfg(feature = "tui")]
fn configured_providers(invocation: &Invocation) -> Vec<String> {
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return Vec::new();
    };
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let Ok(config) = load_config(&root, &working, invocation.config.as_deref()) else {
        return Vec::new();
    };
    config.endpoint_ids()
}

/// Take one answer and say what to ask next.
///
/// Every step validates its own answer and nothing is written until the last
/// one, so abandoning the wizard leaves the configuration exactly as it was.
#[cfg(feature = "tui")]
fn provider_step(
    invocation: &Invocation,
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
        Step::Key => provider_added(invocation, draft, answer),
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
            provider_removed(invocation, &draft.name.clone())
        }
    }
}

/// The first answer: one of the two wizard rows, or an endpoint to switch to.
#[cfg(feature = "tui")]
fn provider_picked(answer: &str, providers: &[String]) -> Result<ProviderNext, String> {
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
fn provider_name(name: String, providers: &[String]) -> Result<String, String> {
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
fn provider_added(
    invocation: &Invocation,
    draft: &tui::ProviderDraft,
    key: &str,
) -> Result<ProviderNext, String> {
    let handle = store_credential(invocation, &draft.name, &draft.store, key)?;
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
fn provider_removed(invocation: &Invocation, name: &str) -> Result<ProviderNext, String> {
    write_config(|config| config_edit::remove_endpoint(config, name))?;
    let store = CatalogStore::resolve(invocation);
    if let Ok(mut records) = catalog(store) {
        let removed: Vec<SecretHandle> = records
            .iter()
            .filter(|record| {
                record.handle.name() == name || record.handle.name() == format!("endpoint.{name}")
            })
            .map(|record| record.handle.clone())
            .collect();
        records.retain(|record| !removed.contains(&record.handle));
        let _ = save_catalog(store, &records);
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
fn forget_credential(handle: &SecretHandle) {
    match handle.store() {
        OS_STORE_ID => {
            let _ = OsCredentialStore.remove(handle.name());
        }
        FILE_STORE_ID => {
            let _ = FileCredentialStore.remove(handle.name());
        }
        _ => {}
    }
}
#[cfg(feature = "tui")]
enum AuthNext {
    Ask(tui::AuthStep),
    Done(String),
    Cancelled(String),
}

#[cfg(feature = "tui")]
fn catalog_handles(invocation: &Invocation) -> Vec<String> {
    catalog(CatalogStore::resolve(invocation))
        .map(|records| records.into_iter().map(|r| r.handle.to_string()).collect())
        .unwrap_or_default()
}

#[cfg(feature = "tui")]
fn auth_step(
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
        tui::AuthStep::Pick => auth_pick(invocation, answer, providers),
        tui::AuthStep::LoginProvider => {
            auth_login_provider(invocation, answer, draft_provider, providers, emitter)
        }
        tui::AuthStep::SetProvider => auth_set_provider(answer, draft_provider, providers),
        tui::AuthStep::SetKey => auth_set_key(invocation, draft_provider, answer),
        tui::AuthStep::RemoveHandle => auth_remove_handle(invocation, answer),
        tui::AuthStep::PasteCode => auth_paste_code(invocation, draft_provider, answer, emitter),
    }
}

#[cfg(feature = "tui")]
fn auth_pick(
    invocation: &Invocation,
    answer: &str,
    providers: &[String],
) -> Result<AuthNext, String> {
    match answer {
        // A built-in preset is always an option, so `login` never dead-ends
        // the way `set` does with nothing configured.
        "login" => Ok(AuthNext::Ask(tui::AuthStep::LoginProvider)),
        "list" => {
            let records = catalog(CatalogStore::resolve(invocation)).map_err(|e| e.message)?;
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
            let records = catalog(CatalogStore::resolve(invocation)).map_err(|e| e.message)?;
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

#[cfg(feature = "tui")]
fn auth_login_provider(
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
        // No loopback listener can catch this issuer's redirect, and no
        // background poll can wait it out either: the operator has to paste
        // a code back, and that has to arrive through this same line editor
        // on a later turn — a blocking stdin read here would compete with
        // it and never see a keystroke.
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

#[cfg(feature = "tui")]
fn auth_set_provider(
    answer: &str,
    draft_provider: &mut String,
    providers: &[String],
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

#[cfg(feature = "tui")]
fn auth_set_key(
    invocation: &Invocation,
    draft_provider: &str,
    answer: &str,
) -> Result<AuthNext, String> {
    store_credential(invocation, draft_provider, "keychain", answer).map_err(|e| e.to_string())?;
    Ok(AuthNext::Done(format!(
        "Stored API key for `{draft_provider}` in the credential store."
    )))
}

#[cfg(feature = "tui")]
fn auth_remove_handle(invocation: &Invocation, answer: &str) -> Result<AuthNext, String> {
    let handle: SecretHandle =
        SecretHandle::try_from(answer.to_owned()).map_err(|error| format!("{error}"))?;
    let store = CatalogStore::resolve(invocation);
    let mut records = catalog(store).map_err(|e| e.message)?;
    records.retain(|r| r.handle != handle);
    save_catalog(store, &records).map_err(|e| e.message)?;
    match handle.store() {
        OS_STORE_ID => {
            let _ = OsCredentialStore.remove(handle.name());
        }
        FILE_STORE_ID => {
            let _ = FileCredentialStore.remove(handle.name());
        }
        _ => {}
    }
    Ok(AuthNext::Done(format!("Removed credential `{handle}`.")))
}

#[cfg(feature = "tui")]
fn auth_paste_code(
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
/// One host serves several models, so the model step takes a list. The first is
/// the endpoint's default; the rest are what `/model` offers beside it.
#[cfg(feature = "tui")]
fn model_slugs(answer: &str) -> Result<Vec<String>, String> {
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

/// Put a typed credential where the operator asked for it, and give back the
/// handle the configuration should point at.
#[cfg(feature = "tui")]
fn store_credential(
    invocation: &Invocation,
    name: &str,
    store: &str,
    secret: &str,
) -> Result<String, String> {
    let secret = secret.trim();
    if secret.len() < arsy_kernel::secret::MIN_SECRET_BYTES {
        return Err("that credential is too short to redact safely".to_owned());
    }
    let handle = if store == "keychain" {
        OsCredentialStore
            .set(name, secret)
            .map_err(|error| format!("the credential store refused it: {error}"))?;
        SecretHandle::new(OS_STORE_ID, name)
    } else {
        let file = format!("{name}.key");
        let path = FileCredentialStore::path(&file)
            .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut written = owner_only(&path).map_err(|error| error.message)?;
        written
            .write_all(secret.as_bytes())
            .map_err(|error| error.to_string())?;
        SecretHandle::new(FILE_STORE_ID, file)
    }
    .map_err(|error| error.to_string())?;

    // Catalogued exactly as `arsy auth set` catalogues one, for two reasons:
    // `auth list` can show it, and every turn registers the catalogued handles
    // for redaction — a credential missing from the catalog is one that could
    // reach output unredacted.
    let store = CatalogStore::resolve(invocation);
    let mut records = catalog(store).map_err(|error| error.message)?;
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
    save_catalog(store, &records).map_err(|error| error.message)?;
    Ok(handle.to_string())
}

/// Rewrite the user configuration through `edit`.
///
/// The file is read and written whole, so `edit` sees exactly what is on disk
/// and nothing it did not change can move.
fn write_config(edit: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
    let path = arsy_kernel::config::user_config()
        .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
    let original = match std::fs::read_to_string(&path) {
        Ok(original) => original,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("the configuration could not be read: {error}")),
    };
    let updated = edit(&original)?;
    replace_file(&path, updated.as_bytes())
        .map_err(|error| format!("the configuration could not be written: {error}"))
}

/// What to print once an effort answer is accepted.
#[cfg(feature = "tui")]
fn effort_line(effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("Effort: {effort}"),
        None => "Effort: off, so no reasoning setting is sent".to_owned(),
    }
}

/// One interactive turn's durable state: the session it is recorded in, and
/// the task it is.
///
/// The TUI takes the same shape `arsy run` does — a turn is a leased task in
/// the session's graph — so an interactive turn that dies with its process is
/// recoverable by `arsy resume` exactly like a scripted one. What it cannot
/// share is `TaskRun::execute`, which owns the streaming loop a terminal has
/// its own version of.
#[cfg(feature = "tui")]
struct RecordedTurn {
    service: AgentService,
    graph: TaskGraph,
    actor: Principal,
    admission: arsy_kernel::service::TurnAdmission,
    session: SessionId,
    task: TaskId,
}

#[cfg(feature = "tui")]
fn record_turn(
    invocation: &Invocation,
    session: SessionId,
    task: String,
    emitter: &mut Emitter,
) -> Result<RecordedTurn, Diagnostic> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
        .map_err(storage_failed)?;
    let mut graph = TaskGraph::new(store, session, actor.clone()).map_err(graph_failed)?;
    let agent = AgentId::new();
    let id = TaskId::new();
    graph
        .add(TaskNode {
            id,
            goal: task.clone(),
            dependencies: Vec::new(),
            assignee: Some(agent),
            required_output: "an answer to the task".to_owned(),
            workspace: WorkspaceRequirement::IsolatedWriter,
            budget: TASK_BUDGET,
            authority: Vec::new(),
            state: TaskState::Pending,
            lease_expires_at_ms: None,
        })
        .map_err(graph_failed)?;
    graph.ready().map_err(graph_failed)?;
    graph
        .lease(id, agent, unix_time_ms() + TASK_LEASE_MS)
        .map_err(graph_failed)?;

    let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: task,
        extensions: Extensions::new(),
    }));
    let admission = service
        .start_turn(actor.clone(), &envelope)
        .map_err(storage_failed)?;
    Ok(RecordedTurn {
        service,
        graph,
        actor,
        admission,
        session,
        task: id,
    })
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn run_turn(
    invocation: &Invocation,
    session_id: SessionId,
    native: Option<&provider::Resolved>,
    task: &str,
    history: &arsy_code::agent::budget::History,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    colour: bool,
    footer: &str,
    conversation: &mut Vec<ModelMessage>,
    transcript: &mut tui::Transcript,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
    emitter: &mut Emitter,
) -> Result<Turn, Diagnostic> {
    let task = prepare_task(invocation, task, emitter)?;
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working, invocation.config.as_deref())?;
    // Loaded once per turn, as `arsy run` loads them: a turn finishes with the
    // hooks it began with.
    let loaded = hook_engine(&root, &config);
    let hooks = (!loaded.is_empty()).then_some(&loaded.engine);
    let task = turn_boundary(
        hooks,
        arsy_code::hook::LifecycleEvent::BeforeTurn,
        &task,
        emitter,
    )
    .map_err(|reason| {
        Diagnostic::error(
            "ARSY-HOK-1001",
            reason,
            "the hook that refused it is listed by `/hooks`",
        )
    })?;
    let RecordedTurn {
        service,
        mut graph,
        actor,
        admission,
        session,
        task: node,
    } = record_turn(invocation, session_id, task.clone(), emitter)?;
    // Where the conversation stood before this turn. A turn that fails or is
    // stopped rewinds to here, which is more than one message once the turn
    // has run tools.
    let base = conversation.len();
    conversation.push(ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text { text: task.clone() }],
    });
    emitter.trace(
        "turn.started",
        json!({
            "turn": admission.turn.to_string(),
            "provider": route.provider,
            "model": route.model,
            "restored_messages": base,
            "cited_events": history.citations.len(),
            "mode": approval.get().label(),
        }),
    );
    let outcome = match native {
        Some(resolved) => native_turn(
            resolved,
            &config,
            &agent_runtime(
                &root,
                &config,
                true,
                &session_id.to_string(),
                Some(session_id),
                Some(session_connector()),
                emitter,
            )?
            .with_execution_mode(approval.get().execution_mode()),
            conversation,
            history,
            route,
            effort,
            admission.turn,
            colour,
            footer,
            keys,
            decoder,
            composer,
            transcript,
            approval,
            hooks,
        ),
        None => external_status(
            &root,
            &task,
            route,
            approval,
            colour,
            footer,
            keys,
            decoder,
            composer,
            &emitter.redactor,
        ),
    };
    // A turn that never started leaves the composer painted, so it is torn down
    // here before the diagnostic is written over the input block.
    if outcome.is_err() {
        let mut stdout = io::stdout();
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
    }
    let turn = match outcome {
        Ok(turn) => turn,
        Err(error) => {
            let reason = format!("could not run {route}: {error}");
            graph
                .fail(node, json!({"message": reason.clone()}))
                .map_err(graph_failed)?;
            fail_turn(
                &service,
                actor,
                admission.turn,
                session,
                route,
                reason,
                emitter,
            )?;
            return Ok(Turn::default());
        }
    };
    if !turn.interrupted && turn.failure.is_none() {
        transcript.push_assistant(&turn.response);
    }
    // Whatever the turn ended as, which is what Codex's `notify` is for.
    // Nothing it returns can change what already happened.
    let stop = match (turn.interrupted, &turn.failure) {
        (true, _) => "interrupted",
        (false, Some(_)) => "failed",
        (false, None) => "answered",
    };
    let _ = turn_boundary(
        hooks,
        arsy_code::hook::LifecycleEvent::AfterTurn,
        stop,
        emitter,
    );
    emitter.trace(
        "turn.finished",
        json!({
            "turn": admission.turn.to_string(),
            "interrupted": turn.interrupted,
            "failed": turn.failure.is_some(),
            "response_bytes": turn.response.len(),
            "usage": turn.usage,
            "messages_added": conversation.len().saturating_sub(base),
        }),
    );
    if turn.interrupted {
        conversation.truncate(base);
        // Stopping a turn is a decision, not a fault: the turn is recorded as
        // failed for the audit trail, but the terminal already said so with an
        // `Interrupted` row and does not need a diagnostic on top.
        service
            .fail_turn(
                actor,
                admission.turn,
                "user_interrupt",
                format!("{route} was interrupted"),
            )
            .map_err(storage_failed)?;
        // Cancelled rather than failed: the operator stopped it, so nothing
        // should offer to continue it later.
        graph
            .cancel(node, "interrupted by the operator")
            .map_err(graph_failed)?;
        turn_record(
            emitter,
            json!({
                "session": session.to_string(),
                "turn": admission.turn.to_string(),
                "status": "interrupted",
            }),
        );
        return Ok(turn);
    }
    match &turn.failure {
        None => {
            if !turn.response.trim().is_empty() {
                conversation.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::Text {
                        text: turn.response.clone(),
                    }],
                });
            }
            let mut outcome = json!({"provider": route.provider, "model": route.model});
            merge(&mut outcome, turn.usage.clone());
            // The interactive path records what it spent for the same reason
            // the scripted one does: `arsy session show` reports one session's
            // totals, and totals that skipped every TUI turn would be fiction.
            let priced = charge_turn(native, &route.model, &turn.usage);
            merge(
                &mut outcome,
                json!({
                    "cost_micros": priced,
                    "cost_source": if priced.is_some() { "configured" } else { "unknown" },
                    "response": turn.response.clone(),
                    // From where this turn began, so a resumed session replays
                    // the tool calls and results the model actually saw rather
                    // than only the two ends of the exchange.
                    "transcript": transcript::persistable(&conversation[base..]),
                }),
            );
            service
                .record_usage(
                    actor.clone(),
                    arsy_kernel::projection::UsageTotals {
                        input_tokens: summary_number(&turn.usage, "input_tokens"),
                        output_tokens: summary_number(&turn.usage, "output_tokens"),
                        cost_micros: priced,
                    },
                )
                .map_err(storage_failed)?;
            // Before the turn is closed, so a process that dies between the
            // two leaves a transcript belonging to a turn that never
            // completed — which the reconstruction ignores — rather than a
            // completed turn whose exchange was never written.
            service
                .record_transcript(
                    actor.clone(),
                    admission.turn,
                    &transcript::persistable(&conversation[base..]),
                )
                .map_err(storage_failed)?;
            service
                .complete_turn(actor, admission.turn, &outcome)
                .map_err(storage_failed)?;
            graph
                .complete(node, outcome.clone())
                .map_err(graph_failed)?;
            turn_record(
                emitter,
                json!({
                    "session": session.to_string(),
                    "turn": admission.turn.to_string(),
                    "status": "completed",
                    "model": route.to_string(),
                }),
            );
        }
        Some(failure) => {
            conversation.truncate(base);
            graph
                .fail(node, json!({"message": failure.clone()}))
                .map_err(graph_failed)?;
            fail_turn(
                &service,
                actor,
                admission.turn,
                session,
                route,
                failure.clone(),
                emitter,
            )?;
        }
    }
    Ok(turn)
}

/// How many times one turn may come back asking to run tools. The bound is
/// what stops a model that answers every result with another call from
/// spending a session on its own loop.
#[cfg(feature = "tui")]
const MAX_TOOL_ROUNDS: usize = 24;

/// What the operator said about one tool call.
#[cfg(feature = "tui")]
fn tool_call_fingerprint(name: &str, arguments: &Value) -> String {
    // A timeout is execution metadata, not command identity. Otherwise a
    // provider can evade duplicate protection by changing only the deadline.
    let identity = if name == "bash" {
        arguments.get("command").cloned().unwrap_or(Value::Null)
    } else {
        arguments.clone()
    };
    format!("{name}\0{identity}")
}
#[cfg(feature = "tui")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum Answer {
    Yes {
        note: Option<String>,
    },
    /// Refuse this call; the turn carries on and can propose something else.
    No {
        note: Option<String>,
    },
    /// Refuse this call and end the turn.
    Stop,
}

/// Run a turn on a configured provider, executing the tools it asks for.
///
/// Each round is one request. A round that ends without tool calls is the
/// answer; a round that asks for tools runs the confirmed ones, appends the
/// call and its result to the conversation, and asks again.
///
/// Nothing runs unconfirmed: every call is shown and answered from the
/// keyboard, and a declined call is reported to the model as a failed result
/// rather than hidden, so it can say what it would do instead.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn native_turn(
    resolved: &provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &mut Vec<ModelMessage>,
    history: &arsy_code::agent::budget::History,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    transcript: &mut tui::Transcript,
    approval: &approval::ApprovalCell,
    hooks: Option<&arsy_code::hook::HookEngine>,
) -> io::Result<Turn> {
    // rather than taken from the last one: an audit that reads a tool-using
    // turn as the price of its final request under-reports what it cost.
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let charge = |outcome: &mut Turn, input: &mut u64, output: &mut u64| {
        *input += outcome.usage["input_tokens"].as_u64().unwrap_or_default();
        *output += outcome.usage["output_tokens"].as_u64().unwrap_or_default();
        if *input > 0 || *output > 0 {
            outcome.usage = json!({"input_tokens": *input, "output_tokens": *output});
        }
    };
    // Successful calls are memoized within this turn. If a provider asks for
    // the exact same effect again, return the first result instead of running
    // it twice or burning all 24 rounds.
    let mut completed_calls = std::collections::HashMap::<String, String>::new();
    for round in 0..MAX_TOOL_ROUNDS {
        // Before the request, not after: a transcript that has outgrown the
        // window fails at the provider, and the operator is told what was
        // elided rather than watching the turn shrink invisibly.
        report_trim(
            colour,
            &arsy_code::agent::budget::fit(conversation, context_budget(resolved), Some(history)),
        )?;
        let mut outcome = native_status(
            resolved,
            config,
            runtime,
            conversation,
            route,
            effort,
            turn,
            round,
            colour,
            footer,
            keys,
            decoder,
            composer,
            approval,
        )?;
        charge(&mut outcome, &mut input_tokens, &mut output_tokens);
        if outcome.calls.is_empty() || outcome.interrupted || outcome.failure.is_some() {
            return Ok(outcome);
        }
        // The calls are history now, whatever the operator decides about them:
        // a provider that sent a call and never sees its result rejects the
        // next request.
        let calls = std::mem::take(&mut outcome.calls);
        let mut content: Vec<ModelContent> = Vec::new();
        if !outcome.response.trim().is_empty() {
            content.push(ModelContent::Text {
                text: outcome.response.clone(),
            });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        conversation.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });

        let mut results = Vec::with_capacity(calls.len());
        let mut terminal = io::stdout();
        let mut all_repeated = true;
        for (id, name, arguments) in &calls {
            let summary = runtime.summarize(name, arguments);
            let fingerprint = tool_call_fingerprint(name, arguments);
            let cached = completed_calls.get(&fingerprint).cloned();
            let repeated = cached.is_some();
            // Once the turn is stopped the remaining calls are still answered,
            // because a call the provider sent needs a result; they are simply
            // answered without running anything.
            let (content, is_error) = match (outcome.interrupted, cached) {
                // Once the turn is stopped the remaining calls are still
                // answered, because a call the provider sent needs a result;
                // they are simply answered without running anything.
                (true, _) => {
                    all_repeated = false;
                    ("The operator declined to run this call.".to_owned(), true)
                }
                (false, Some(previous)) => (
                    format!(
                        "This exact tool call already completed successfully; skipped duplicate.\n\
                         {previous}"
                    ),
                    false,
                ),
                (false, None) => {
                    all_repeated = false;
                    run_call(
                        runtime,
                        &mut terminal,
                        colour,
                        &summary,
                        Call {
                            name,
                            arguments,
                            fingerprint,
                        },
                        Answering {
                            keys,
                            decoder,
                            approval,
                            completed: &mut completed_calls,
                            interrupted: &mut outcome.interrupted,
                            hooks,
                        },
                    )?
                }
            };
            if !repeated {
                transcript.push_tool(
                    name,
                    &summary,
                    &content,
                    !is_error,
                    std::time::Duration::from_millis(50),
                );
            }
            let card = if repeated {
                tui::tool_result_row(colour, name, true, "duplicate skipped")
            } else {
                tui::tool_card(
                    tui::terminal_width(),
                    colour,
                    name,
                    &summary,
                    &content,
                    !is_error,
                    std::time::Duration::from_millis(50),
                )
            };
            writeln!(
                terminal,
                "{}{}{}",
                tui::DISABLE_AUTOWRAP,
                card,
                tui::ENABLE_AUTOWRAP
            )?;
            terminal.flush()?;
            results.push(ModelContent::ToolResult {
                id: id.clone(),
                content,
                is_error,
            });
        }
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
        if all_repeated && !calls.is_empty() {
            outcome.response =
                "The requested operation already completed; a repeated tool call was skipped."
                    .to_owned();
            return Ok(outcome);
        }
        // The response of a round that called tools belongs to the history
        // above, not to the answer this turn returns.
        outcome.response.clear();
        if outcome.interrupted {
            return Ok(outcome);
        }
        if round + 1 == MAX_TOOL_ROUNDS {
            outcome.failure = Some(format!(
                "{route} asked for tools {MAX_TOOL_ROUNDS} times without finishing the turn"
            ));
            return Ok(outcome);
        }
    }
    Ok(Turn::default())
}

/// Take the terminal's size again, no more than ten times a second.
///
/// Answers whether it was measured on this pass, because the rows on screen
/// were laid out for the size before it.
#[cfg(feature = "tui")]
fn remeasure(
    painter: &Painter<'_>,
    composer: &mut tui::Composer,
    refreshed: &mut std::time::Instant,
) -> bool {
    if refreshed.elapsed() < std::time::Duration::from_millis(100) {
        return false;
    }
    painter.width.set(tui::terminal_width());
    composer.set_height(tui::terminal_rows());
    *refreshed = std::time::Instant::now();
    true
}

/// Start the provider and wire its three streams.
///
/// Stderr and the task being written are each read on their own thread, and
/// the event stream on a third, so the main loop can watch the keyboard while
/// the provider works — which is what lets Esc stop a turn and keeps the
/// composer typeable.
#[cfg(feature = "tui")]
type ProviderStreams = (
    tui::ProviderChild,
    std::sync::mpsc::Receiver<String>,
    std::sync::mpsc::Receiver<io::Result<()>>,
    std::sync::mpsc::Receiver<io::Result<String>>,
);

#[cfg(feature = "tui")]
fn spawn_provider(mut command: std::process::Command, task: &str) -> io::Result<ProviderStreams> {
    // A process group of its own, so a signal aimed at the harness does not also
    // reach the provider child. There is no Windows equivalent to gate on.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut child = tui::ProviderChild(child);
    let mut stderr = child.0.stderr.take().expect("piped stderr is available");
    let (errors, error_output) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = Read::by_ref(&mut stderr).take(8192).read_to_end(&mut bytes);
        let _ = io::copy(&mut stderr, &mut io::sink());
        let _ = errors.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let mut stdin = child.0.stdin.take().expect("piped stdin is available");
    let task = task.to_owned();
    let (sent, input) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sent.send(stdin.write_all(task.as_bytes()));
    });
    // The event stream is read on a thread so the main loop can also watch the
    // key stream: that is what lets Esc or Ctrl-C stop a turn, and what keeps
    // the composer alive and typeable while the provider works.
    let stdout = child.0.stdout.take().expect("piped stdout is available");
    let events = tui::provider_lines(stdout);
    Ok((child, error_output, input, events))
}

/// The line an action carries, if it carries one.
#[cfg(feature = "tui")]
fn submitted(
    input: tui::Action,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
) -> Option<String> {
    match input {
        tui::Action::Submit(line) => Some(line),
        tui::Action::CycleMode => {
            let mode = cycle_approval_mode(approval);
            state.set_approval_mode(mode.label());
            None
        }
        tui::Action::Quit | tui::Action::Redraw | tui::Action::None => None,
    }
}

/// Whether the answer being typed is a credential, which is shown as bullets,
/// never painted into the scrollback, and never remembered.
#[cfg(feature = "tui")]
fn masked(prompt: &Prompt) -> bool {
    matches!(prompt, Prompt::Provider(step) if step.masked())
        || matches!(prompt, Prompt::Auth(step) if step.masked())
}

/// Whether ending input here closes a picker rather than the session.
///
/// Every picker has to be named, or leaving one exits ARSY instead.
#[cfg(feature = "tui")]
fn cancels_to_task(prompt: &Prompt) -> bool {
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
fn answer_prompt(
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
            step,
            line,
            typing.auth_draft,
            typing.providers,
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
fn take_resume(
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
#[cfg(feature = "tui")]
enum TaskPass {
    /// Carry on at the task prompt.
    Go,
    /// Collect the next answer at this prompt instead.
    Ask(Prompt),
    Stop,
}

/// What a line typed at the task prompt can reach, apart from the session
/// itself.
#[cfg(feature = "tui")]
struct Typing<'a> {
    workspace: &'a Path,
    colour: bool,
    provider_available: bool,
    route: &'a mut tui::ModelRoute,
    effort: &'a mut Option<Effort>,
    models: &'a mut Vec<tui::ModelChoice>,
    providers: &'a mut Vec<String>,
    chosen: &'a mut Option<String>,
    draft: &'a mut tui::ProviderDraft,
    auth_draft: &'a mut String,
    theme: &'a mut String,
    roles: &'a std::collections::BTreeMap<String, String>,
    sessions: &'a mut Vec<tui::SessionChoice>,
    resolved_providers: &'a mut std::collections::HashMap<String, provider::Resolved>,
    unavailable_providers: &'a mut std::collections::HashSet<String>,
}

/// Answer a line typed at the task prompt.
///
/// A slash command is answered here; anything else is the task itself and is
/// sent to the model.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn answer_task(
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
    if matches!(line.trim(), ":quit" | "/quit" | "/exit") {
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        return Ok(TaskPass::Stop);
    }
    if line.trim().starts_with('/') || line.trim().is_empty() {
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        // `/mcp` alone opens the dialog, which writes the configuration; with
        // any argument it is the read-only inspection it always was.
        if line.trim() == "/mcp" {
            run_mcp_dialog(invocation, stdout, typing.colour, keys, decoder)?;
            return Ok(TaskPass::Go);
        }
        return slash_command(line, invocation, typing, restoring, stdout, emitter);
    }
    run_task(
        line, invocation, typing, restoring, stdout, keys, decoder, composer, emitter,
    )
}

/// Answer a slash command typed at the task prompt.
#[cfg(feature = "tui")]
fn slash_command(
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
fn run_task(
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
    if !typing.provider_available {
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
struct Running<'a> {
    workspace: &'a Path,
    route: &'a tui::ModelRoute,
    effort: Option<Effort>,
    colour: bool,
    state: &'a mut tui::TuiState,
    conversation: &'a mut Vec<ModelMessage>,
    transcript: &'a mut tui::Transcript,
    history: &'a arsy_code::agent::budget::History,
    approval: &'a approval::ApprovalCell,
    queued: &'a mut std::collections::VecDeque<String>,
}

/// Run one turn and settle what it left behind.
///
/// Answers whether the session carries on: a turn can end it, and nothing
/// after that should run.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn take_turn(
    invocation: &Invocation,
    resolved: Option<&provider::Resolved>,
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
fn inspect_command(
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
///
/// A rejected `[theme]` override is reported and dropped, never left to blank
/// the screen.
#[cfg(feature = "tui")]
fn open_palette(
    invocation: &Invocation,
    workspace: &Path,
    emitter: &mut Emitter,
) -> (arsy_kernel::config::Theme, String) {
    let config = load_config(workspace, workspace, invocation.config.as_deref())
        .map(|config| config.theme().clone())
        .unwrap_or_default();
    let (theme, palette) = resolve_palette(&config);
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
    (config, theme)
}

/// What the session already knows about its providers before the first turn.
///
/// A failed native resolution is remembered as unavailable so it is not probed
/// again on every turn while the external Codex login is still working.
#[cfg(feature = "tui")]
fn seed_providers(
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
struct Opened {
    native: Option<provider::Resolved>,
    /// What was asked for, which is not always what resolved.
    native_requested: Option<String>,
    detected: tui::ModelRoute,
    provider_available: bool,
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
fn open_route(invocation: &Invocation, workspace: &Path) -> Result<Opened, Diagnostic> {
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
    let detected = match &native {
        Some((resolved, model)) => Some(tui::ModelRoute {
            provider: resolved.endpoint.id.clone(),
            model: model.clone(),
        }),
        None => tui::detect_model_route(),
    };
    // Nothing configured and no Codex login is not fatal: the session still
    // opens so `/mcp` and `/hooks` can inspect the workspace. Only a task turn
    // is refused, which `provider_available` gates below.
    let provider_available = detected.is_some();
    let detected = detected.unwrap_or_else(|| tui::ModelRoute {
        provider: tui::CODEX_PROVIDER.to_owned(),
        model: "default".into(),
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
fn steers_turn(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("/plan" | "/todo" | "/approval")
    )
}

#[cfg(feature = "tui")]
fn steer_turn(
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
        Some("/approval") => set_mode(line, approval, state, stdout),
        _ => Ok(()),
    }
}

/// Enter, revise, approve, cancel or show the plan.
#[cfg(feature = "tui")]
fn plan_step(
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
fn show_todos(
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

/// Take the approval mode a command named, or report the one in force.
#[cfg(feature = "tui")]
fn set_mode(
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

/// Drive the session dialog until it is answered or left.
///
/// The dialog owns the keyboard while it is open: it is a list in front of the
/// reader, and every key belongs to it until it closes.
#[cfg(feature = "tui")]
fn run_session_dialog(
    mut dialog: tui::SessionDialogState,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let width = tui::terminal_width();
    writeln!(stdout, "{}", dialog.render(width, colour)).map_err(terminal_failed)?;
    stdout.flush().map_err(terminal_failed)?;
    loop {
        // A keyboard that hung up leaves the dialog, rather than holding the
        // session on a list nothing can answer.
        let Ok(byte) = keys.recv() else {
            return Ok(());
        };
        let Some(action) = decoder.feed(byte).and_then(|key| dialog.handle_key(key)) else {
            write!(stdout, "\r\x1b[J{}\n", dialog.render(width, colour))
                .map_err(terminal_failed)?;
            stdout.flush().map_err(terminal_failed)?;
            continue;
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
                let loaded = resume_into(id, borrowed);
                writeln!(stdout, "Resumed session {id} ({loaded} message(s) loaded).")
                    .map_err(terminal_failed)?;
            }
            tui::SessionAction::Rename(id, title) => {
                if let Ok(store) = open_store(borrowed.workspace) {
                    let _ = store.set_session_title(id, &title);
                }
                writeln!(stdout, "Renamed session {id} to \"{title}\".")
                    .map_err(terminal_failed)?;
            }
            tui::SessionAction::Delete(id) => {
                delete_session(Some(&id.to_string()), borrowed, stdout)?;
            }
            tui::SessionAction::Cancel => {}
        }
        return Ok(());
    }
}

/// Every MCP connection ARSY defines, then every one another tool declares
/// under a name ARSY does not already use, with the entries they came from.
#[cfg(feature = "tui")]
fn mcp_choices(
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

/// Drive the `/mcp` dialog until it is closed.
///
/// Each key redraws the frame over the last one rather than under it, and the
/// frame is erased on close, so moving through the list leaves nothing behind;
/// only a line per change made stays in the scrollback.
#[cfg(feature = "tui")]
fn run_mcp_dialog(
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let mut rows = mcp_choices(&root, invocation)?;
    let mut dialog = tui::McpDialogState::new(rows.iter().map(|(_, c)| c.clone()).collect());
    let mut changes: Vec<String> = Vec::new();
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
                write!(stdout, "\x1b[{drawn}A\r\x1b[J").map_err(terminal_failed)?;
                if !changes.is_empty() {
                    changes.push("MCP changes take effect from the next turn.".to_owned());
                    writeln!(stdout, "{}", tui::safe_text(&changes.join("\n")))
                        .map_err(terminal_failed)?;
                }
                return Ok(());
            }
            Some(action) => action,
        };
        dialog.notice = Some(match apply_mcp_action(&root, &rows, &dialog, action) {
            Ok(change) => {
                changes.push(change.clone());
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
fn next_mcp_action(
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
fn apply_mcp_action(
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

/// The slash commands that act on the recorded session rather than on the
/// conversation with the model.
#[cfg(feature = "tui")]
fn manages_session(line: &str) -> bool {
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
fn manage_session(
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
fn resume_command(
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
fn rename_session(
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
fn delete_session(
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
fn start_session(restoring: Restoring<'_>) -> SessionId {
    let started = SessionId::new();
    restoring.state.set_session_id(started);
    restoring.conversation.clear();
    restoring.transcript.clear();
    *restoring.history = arsy_code::agent::budget::History::default();
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
fn opens_picker(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("/auth" | "/model" | "/provider" | "/effort" | "/theme")
    )
}

/// What opening a picker re-reads or resets.
#[cfg(feature = "tui")]
struct Opening<'a> {
    models: &'a mut Vec<tui::ModelChoice>,
    providers: &'a mut Vec<String>,
    chosen: &'a mut Option<String>,
    draft: &'a mut tui::ProviderDraft,
    auth_draft: &'a mut String,
    effort: &'a mut Option<Effort>,
    theme: &'a mut String,
    roles: &'a std::collections::BTreeMap<String, String>,
    state: &'a mut tui::TuiState,
}

/// Open the picker a slash command names, or take the answer it carried.
///
/// `Some` is the prompt that now collects the answer; `None` means the line
/// answered outright and the task prompt stays.
#[cfg(feature = "tui")]
fn open_picker(
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
            // Re-read, so a model added to any endpoint since startup is
            // offered without restarting.
            let mut models = endpoint_models(invocation);
            models.extend(tui::available_models());
            *opening.models = models;
            Ok(Some(Prompt::Model))
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
struct Restoring<'a> {
    workspace: &'a Path,
    state: &'a mut tui::TuiState,
    conversation: &'a mut Vec<ModelMessage>,
    transcript: &'a mut tui::Transcript,
    history: &'a mut arsy_code::agent::budget::History,
    approval: &'a approval::ApprovalCell,
    queued: &'a mut std::collections::VecDeque<String>,
}

/// Open a recorded session, answering how many messages it carried.
///
/// The approval mode goes back to default and the queue is dropped: both
/// belonged to the session being left, and carrying either into another one
/// would give it authority nobody granted it there.
#[cfg(feature = "tui")]
fn resume_into(session: SessionId, restoring: Restoring<'_>) -> usize {
    let (conversation, history) = reconstruct_session_conversation(restoring.workspace, session);
    *restoring.conversation = conversation;
    *restoring.history = history;
    restoring.transcript.clear();
    restoring.state.set_session_id(session);
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
struct Leaving<'a> {
    effort: Option<Effort>,
    theme: &'a str,
    roles: &'a std::collections::BTreeMap<String, String>,
    draft: &'a mut tui::ProviderDraft,
    auth_draft: &'a mut String,
    session: SessionId,
    route: &'a tui::ModelRoute,
}

/// Close a picker without taking an answer, and say what is still in force.
///
/// Ending input at a picker cancels the picker, not the session: the setting
/// is unchanged and the task prompt returns.
#[cfg(feature = "tui")]
fn leave_picker(prompt: &Prompt, leaving: Leaving<'_>) -> String {
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
fn resolve_route<'a>(
    invocation: &Invocation,
    workspace: &Path,
    provider: &str,
    resolved: &'a mut std::collections::HashMap<String, provider::Resolved>,
    unavailable: &mut std::collections::HashSet<String>,
) -> Option<&'a provider::Resolved> {
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
    resolved.get(provider)
}

/// Ask the operator what to do with the plan a planning turn produced.
///
/// The structured plan is preferred over the prose, because that is what the
/// harness recorded; the prose stands in only when no steps were written.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn settle_plan(
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
        tui::AskDialogResult::AlwaysApprove { note } => {
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
#[allow(clippy::too_many_arguments)]
fn take_provider(
    invocation: &Invocation,
    step: tui::ProviderStep,
    line: &str,
    draft: &mut tui::ProviderDraft,
    providers: &mut Vec<String>,
    chosen: &mut Option<String>,
    stdout: &mut io::Stdout,
) -> Result<Prompt, Diagnostic> {
    let message = match provider_step(invocation, step, line, draft, providers) {
        Ok(ProviderNext::Ask(next)) => return Ok(Prompt::Provider(next)),
        Ok(ProviderNext::Done(message)) => {
            writeln!(stdout, "{}", tui::safe_text(&message)).map_err(terminal_failed)?;
            *providers = configured_providers(invocation);
            *chosen = configured_default(invocation);
            *draft = tui::ProviderDraft::default();
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
    Ok(Prompt::Task)
}

/// Take the reasoning effort the operator picked.
///
/// A rejected answer leaves the list open so it can be retyped against what is
/// already on screen.
#[cfg(feature = "tui")]
fn take_effort(
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
fn take_model(
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
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn take_auth(
    invocation: &Invocation,
    step: tui::AuthStep,
    line: &str,
    draft: &mut String,
    providers: &[String],
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<Prompt, Diagnostic> {
    let (message, next) = match auth_step(invocation, step, line, draft, providers, emitter) {
        Ok(AuthNext::Ask(next)) => return Ok(Prompt::Auth(next)),
        // Finished or abandoned, the draft goes either way: a credential is
        // never left in memory for the next question to pick up.
        Ok(AuthNext::Done(message) | AuthNext::Cancelled(message)) => {
            draft.clear();
            (message, Prompt::Task)
        }
        Err(reason) => (reason, Prompt::Auth(step)),
    };
    writeln!(stdout, "{}", tui::safe_text(&message)).map_err(terminal_failed)?;
    Ok(next)
}

/// What the pickers read to draw themselves.
#[cfg(feature = "tui")]
struct Picker<'a> {
    state: &'a tui::TuiState,
    workspace: &'a Path,
    models: &'a [tui::ModelChoice],
    route: &'a tui::ModelRoute,
    effort: Option<Effort>,
    theme: &'a str,
    draft: &'a tui::ProviderDraft,
    auth_draft: &'a str,
    sessions: &'a [tui::SessionChoice],
    providers: &'a [String],
    chosen_provider: Option<&'a str>,
}

/// The line under the composer: the status row, or whatever the open picker
/// wants said above its rows.
#[cfg(feature = "tui")]
fn prompt_status(prompt: &Prompt, picker: Picker<'_>, colour: bool) -> String {
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
fn offer_rows(
    prompt: &Prompt,
    composer: &mut tui::Composer,
    picker: Picker<'_>,
    invocation: &Invocation,
) {
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
            let handles = catalog_handles(invocation);
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

/// The parts of a running turn an event can change.
#[cfg(feature = "tui")]
struct Streamlined<'a> {
    outcome: &'a mut Turn,
    finished: &'a mut Option<std::time::Instant>,
    stopped_early: &'a mut bool,
    seen_git: &'a mut std::collections::HashSet<String>,
    /// The last row drawn, so an event that renders the same twice is drawn
    /// once.
    last_row: &'a mut Option<String>,
}

/// Read one line from the provider and draw what it says.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn show_event(
    line: &str,
    redactor: &Redactor,
    child: &mut tui::ProviderChild,
    painter: &Painter<'_>,
    terminal: &mut io::Stdout,
    composer: &mut tui::Composer,
    run: Streamlined<'_>,
    colour: bool,
    tick: usize,
) -> io::Result<()> {
    let line = redactor.sanitize(line).map_err(io::Error::other)?;
    let event = serde_json::from_str::<Value>(&line)
        .map_err(|_| io::Error::other("provider emitted invalid JSON"))?;
    absorb_event(&event, run.outcome, run.finished);
    let repeated_git = run.finished.is_none() && repeated_git(&event, run.seen_git);
    if repeated_git {
        *run.stopped_early = true;
        *run.finished = Some(std::time::Instant::now());
        child.stop(false);
        note_repeated_git(run.outcome);
        painter.row(
            terminal,
            composer,
            Some(&tui::tool_result_row(
                colour,
                "git",
                true,
                "repeated successful command skipped",
            )),
            false,
            run.outcome.queued.len(),
            tick,
        )?;
    }
    // A killed provider still flushes buffered events; showing them
    // after the interrupt notice would contradict it.
    if !run.outcome.interrupted && !repeated_git {
        if let Some(row) = tui::render_codex_event(&line, colour) {
            if run.last_row.as_ref() != Some(&row) {
                painter.row(
                    terminal,
                    composer,
                    Some(&row),
                    false,
                    run.outcome.queued.len(),
                    tick,
                )?;
            }
            *run.last_row = Some(row);
        }
    }
    Ok(())
}

/// Say in the answer that a repeated Git command was stopped.
///
/// The turn ends here, so the reason has to reach the model in the answer
/// itself: the row on screen is for the operator, not for the next request.
#[cfg(feature = "tui")]
fn note_repeated_git(outcome: &mut Turn) {
    if !outcome.response.is_empty() {
        outcome.response.push_str("\n\n");
    }
    outcome
        .response
        .push_str("The provider repeated a successful Git command; the duplicate was skipped.");
}

/// Take what an event says about the turn.
///
/// The provider's own words are the answer; a terminal event settles when the
/// turn ended, whatever the process does afterwards.
#[cfg(feature = "tui")]
fn absorb_event(event: &Value, outcome: &mut Turn, finished: &mut Option<std::time::Instant>) {
    if finished.is_none()
        && matches!(
            event["type"].as_str(),
            Some("turn.completed" | "turn.failed")
        )
    {
        *finished = Some(std::time::Instant::now());
    }
    outcome.provider_failed |= event["type"] == "turn.failed";
    if event["type"] != "item.completed" || event["item"]["type"] != "agent_message" {
        return;
    }
    if let Some(text) = event["item"]["text"].as_str() {
        if !outcome.response.is_empty() {
            outcome.response.push('\n');
        }
        outcome.response.push_str(text);
    }
}

/// Whether the turn's loop carries on.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum Pass {
    Go,
    Stop,
}

/// The clocks a running provider turn watches.
#[cfg(feature = "tui")]
struct Clocks<'a> {
    started: std::time::Instant,
    status: &'a mut Option<std::process::ExitStatus>,
    /// When the process was seen to have left.
    exited: &'a mut Option<std::time::Instant>,
    /// When the provider said the turn was over.
    finished: &'a mut Option<std::time::Instant>,
    last_event: std::time::Instant,
    /// When a stop was asked for, so it can be escalated.
    cancelling: Option<std::time::Instant>,
    stopped_early: &'a mut bool,
}

/// Where the provider's process stands at the top of a pass.
///
/// The turn is over when the provider says it is over. A CLI that lingers
/// after its terminal event — cleaning up a session, flushing telemetry —
/// must not keep the clock running against an answer already on screen.
#[cfg(feature = "tui")]
fn lifecycle(child: &mut tui::ProviderChild, clocks: Clocks<'_>) -> io::Result<Pass> {
    if clocks.status.is_none() {
        *clocks.status = child.0.try_wait()?;
        if clocks.status.is_some() {
            *clocks.exited = Some(std::time::Instant::now());
            child.stop(true);
        }
    }
    if clocks
        .exited
        .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
    {
        return Ok(Pass::Stop);
    }
    // The turn is over when the provider says it is over. A CLI that
    // lingers after its terminal event — cleaning up a session, flushing
    // telemetry — must not keep the clock running against the answer that
    // is already on screen.
    //
    // Trailing rows still land: the stream drains until it has been quiet
    // for 250ms, and no longer than 2 seconds however talkative it stays.
    if clocks.status.is_none()
        && clocks.finished.is_some_and(|at: std::time::Instant| {
            clocks.last_event.elapsed() >= std::time::Duration::from_millis(250)
                || at.elapsed() >= std::time::Duration::from_secs(2)
        })
    {
        *clocks.stopped_early = true;
        child.stop(false);
        return Ok(Pass::Stop);
    }
    if clocks.started.elapsed() >= std::time::Duration::from_secs(300) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "provider exceeded the 300-second turn deadline",
        ));
    }
    if clocks
        .cancelling
        .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
    {
        child.stop(true);
        *clocks.status = Some(child.0.wait()?);
        return Ok(Pass::Stop);
    }
    Ok(Pass::Go)
}

/// The call a live view is watching.
#[cfg(feature = "tui")]
struct LiveCall<'a> {
    name: &'a str,
    /// Only a process can be cancelled part way; everything else runs to its
    /// own end and Ctrl-C would leave the workspace half changed.
    cancellable: bool,
    operation_id: arsy_kernel::domain::OperationId,
}

/// Take the keys pressed while a call runs, answering whether it was
/// cancelled.
///
/// `e` toggles how much of the output is shown. Every other key belongs to
/// the composer and is read once the call is done.
#[cfg(feature = "tui")]
fn absorb_live_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    terminal: &mut io::Stdout,
    call: LiveCall<'_>,
    drawn_rows: &mut usize,
    expanded: &mut bool,
) -> io::Result<bool> {
    let mut cancelled = false;
    for key in keys.try_iter().filter_map(|byte| decoder.feed(byte)) {
        match key {
            tui::Key::Interrupt if call.cancellable => {
                arsy_code::process::cancel(call.operation_id);
                if *drawn_rows > 0 {
                    write!(terminal, "\x1b[{}A\r\x1b[J", drawn_rows)?;
                    *drawn_rows = 0;
                }
                write!(terminal, "\r\x1b[K  ✦ Cancelling {}…\n", call.name)?;
                terminal.flush()?;
                cancelled = true;
            }
            tui::Key::Char('e' | 'E') => *expanded = !*expanded,
            _ => {}
        }
    }
    Ok(cancelled)
}

/// Take whatever a running command has printed since the last pass.
///
/// The tail is what a reader needs while it runs, so the buffer is capped and
/// the oldest output is dropped rather than growing without bound.
#[cfg(feature = "tui")]
fn absorb_output(output: &std::sync::mpsc::Receiver<String>, live: &mut String) {
    /// What is kept of a long-running command's output.
    const KEEP_BYTES: usize = 16_384;

    live.extend(output.try_iter());
    if live.len() > KEEP_BYTES {
        let oldest = live.len() - KEEP_BYTES;
        live.drain(..oldest);
    }
}

/// What a key press during a provider turn can reach.
#[cfg(feature = "tui")]
struct Keyboard<'a> {
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    composer: &'a mut tui::Composer,
    approval: &'a approval::ApprovalCell,
}

/// The turn those keys can change.
#[cfg(feature = "tui")]
struct Turning<'a> {
    outcome: &'a mut Turn,
    cancelling: &'a mut Option<std::time::Instant>,
    last_key: &'a mut std::time::Instant,
}

/// Take the keys waiting, without blocking on the next one.
///
/// Bounded per pass so a held key cannot starve the event stream: whatever is
/// still waiting is read on the pass after this one.
///
/// Answers whether anything typed changed what is on screen.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn provider_keys(
    board: Keyboard<'_>,
    terminal: &mut io::Stdout,
    painter: &Painter<'_>,
    child: &mut tui::ProviderChild,
    turning: Turning<'_>,
    colour: bool,
    tick: usize,
) -> io::Result<bool> {
    let mut typed = false;
    for _ in 0..256 {
        let byte = match board.keys.try_recv() {
            Ok(byte) => byte,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                turning.outcome.quit = true;
                stop_turn(turning.outcome, child, turning.cancelling);
                break;
            }
        };
        *turning.last_key = std::time::Instant::now();
        let Some(key) = board.decoder.feed(byte) else {
            continue;
        };
        // While the provider is running, Interrupt always means the turn,
        // never the composer or the session — and it drops a queued
        // follow-up, which was only queued to run after this turn.
        if key == tui::Key::Interrupt {
            if stop_turn(turning.outcome, child, turning.cancelling) {
                painter.row(
                    terminal,
                    board.composer,
                    Some(&tui::interrupted_row(colour)),
                    true,
                    0,
                    tick,
                )?;
            }
            continue;
        }
        match board.composer.press(key) {
            // Shift+Tab is an immediate mode change, not a follow-up task.
            // Keeping it out of the queue prevents a drafted chat line from
            // being answered as if it were a second user message.
            tui::Action::CycleMode => {
                cycle_approval_mode(board.approval);
                typed = true;
            }
            // A line sent while the provider is busy runs as soon as this
            // turn ends, rather than being dropped or blocking.
            tui::Action::Submit(line) if !line.trim().is_empty() => {
                if turning.outcome.queued.len() < 16 {
                    // Queued, not dropped: the row below says it was
                    // taken, and the turn that follows this one runs it.
                    turning.outcome.queued.push_back(line);
                    painter.row(
                        terminal,
                        board.composer,
                        Some("  Follow-up queued."),
                        turning.cancelling.is_some(),
                        turning.outcome.queued.len(),
                        tick,
                    )?;
                } else {
                    board.composer.restore(line);
                    painter.row(
                        terminal,
                        board.composer,
                        Some("  Queue full; draft retained."),
                        turning.cancelling.is_some(),
                        turning.outcome.queued.len(),
                        tick,
                    )?;
                }
            }
            tui::Action::Submit(_) => typed = true,
            tui::Action::Quit => {
                turning.outcome.quit = true;
                stop_turn(turning.outcome, child, turning.cancelling);
            }
            tui::Action::Redraw => typed = true,
            tui::Action::None => {}
        }
    }
    Ok(typed)
}

/// Draws the rows a provider turn produces, above the live composer.
#[cfg(feature = "tui")]
struct Painter<'a> {
    colour: bool,
    footer: &'a str,
    /// Re-measured on the resize tick rather than per row.
    width: std::cell::Cell<usize>,
    started: std::time::Instant,
}

#[cfg(feature = "tui")]
impl Painter<'_> {
    /// One row, or none — either way the status under it is repainted.
    ///
    /// The composer is torn down and drawn again around each row, so the input
    /// block is never overwritten by what lands above it.
    fn row(
        &self,
        terminal: &mut io::Stdout,
        composer: &mut tui::Composer,
        row: Option<&str>,
        cancelling: bool,
        queued: usize,
        tick: usize,
    ) -> io::Result<()> {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        let phase = if cancelling {
            tui::TurnPhase::Cancelling
        } else {
            tui::TurnPhase::Working
        };
        let status = tui::turn_status(self.colour, phase, self.started.elapsed(), tick, queued);
        frame.push_str(&composer.render_turn(self.width.get(), self.colour, &status, self.footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    }
}

/// What the provider's exit says about the turn, once the turn itself is done.
///
/// A zero process exit must not mask a turn the provider reported as failed,
/// so the event stream is read before the exit status.
#[cfg(feature = "tui")]
fn verdict(
    child: &mut tui::ProviderChild,
    route: &tui::ModelRoute,
    status: Option<std::process::ExitStatus>,
    stopped_early: bool,
    outcome: &Turn,
) -> io::Result<Option<String>> {
    let status = match status {
        Some(status) => status,
        // The turn ended before the process did, so the process is asked to
        // leave and then made to: waiting on a CLI that ignores the signal is
        // the hang this exit was added to avoid.
        None if stopped_early => reap(child)?,
        None => child.0.wait()?,
    };
    Ok(if outcome.provider_failed {
        Some(format!("{route} reported a failed turn"))
    // A signal ARSY sent after a completed turn is its own exit code, not a
    // verdict on the turn the provider already reported.
    } else if status.success() || stopped_early {
        None
    } else {
        Some(format!("{route} exited with status {status}"))
    })
}

/// Wait briefly for a provider asked to leave, then make it.
#[cfg(feature = "tui")]
fn reap(child: &mut tui::ProviderChild) -> io::Result<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            child.stop(true);
            return child.0.wait();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The error for a provider that closed its stream without ever saying the
/// turn ended, carrying whatever it wrote to stderr.
#[cfg(feature = "tui")]
fn silent_provider(
    errors: &std::sync::mpsc::Receiver<String>,
    redactor: &Redactor,
) -> io::Result<io::Error> {
    let detail = errors
        .recv_timeout(std::time::Duration::from_millis(100))
        .unwrap_or_default();
    let detail = redactor.sanitize(&detail).map_err(io::Error::other)?;
    Ok(io::Error::other(format!(
        "provider closed its stream without a terminal turn event: {}",
        terminal_text(detail.trim())
    )))
}

/// Whether this event is a Git command that already succeeded this turn.
///
/// A provider that repeats a push or a commit would run it twice, so the
/// duplicate is caught at its start event — before it gets a second chance —
/// which means the set is filled by the completions that came before it.
#[cfg(feature = "tui")]
fn repeated_git(event: &Value, seen: &mut std::collections::HashSet<String>) -> bool {
    if event["item"]["type"] != "command_execution" {
        return false;
    }
    let Some(command) = event["item"]
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| command.contains("git "))
    else {
        return false;
    };
    match event["type"].as_str() {
        Some("item.started") => seen.contains(command),
        Some("item.completed") if event["item"]["exit_code"].as_i64() == Some(0) => {
            !seen.insert(command.to_owned())
        }
        _ => false,
    }
}

/// Stop the running turn.
///
/// The queue goes with it: a follow-up was only queued to run after this turn,
/// and the operator stopping the turn is not asking for the next one. The
/// moment the stop began is kept so an unresponsive provider can be escalated
/// from a polite stop to a kill.
///
/// Answers whether this call was the one that started the stop, because that
/// is when the row saying so is drawn — a second Ctrl-C must not draw it again.
#[cfg(feature = "tui")]
fn stop_turn(
    outcome: &mut Turn,
    child: &mut tui::ProviderChild,
    cancelling: &mut Option<std::time::Instant>,
) -> bool {
    outcome.queued.clear();
    outcome.interrupted = true;
    if cancelling.is_some() {
        return false;
    }
    *cancelling = Some(std::time::Instant::now());
    child.stop(false);
    true
}

/// What a streaming round has put on screen so far.
///
/// Reasoning and the answer hold separate buffers and separate boxes, so the
/// verbose stream reads as distinct parts of the turn rather than one grey
/// blur. Deltas arrive token by token and a row is drawn per line, so what is
/// left of an unfinished line is kept here until the rest of it arrives.
#[cfg(feature = "tui")]
#[derive(Default)]
struct Streaming {
    /// Answer text not yet ended by a line break.
    pending: String,
    /// Reasoning not yet ended by a line break.
    thinking: String,
    thinking_open: bool,
    answer_open: bool,
    /// Rows drawn for `pending`, which a finished line replaces.
    live_lines: usize,
}

#[cfg(feature = "tui")]
impl Streaming {
    /// Draw reasoning as it streams, opening its box on the first delta.
    fn reason(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        let width = tui::terminal_width();
        if !self.thinking_open {
            self.thinking_open = true;
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_top(width, colour),
            )?;
        }
        self.thinking.push_str(text);
        for line in drain_lines(&mut self.thinking) {
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_row(width, colour, &line),
            )?;
        }
        Ok(())
    }

    /// Draw the answer as it streams. Answer text closes the reasoning box
    /// first, so the prose never starts inside it.
    fn answer(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        if !self.answer_open {
            self.answer_open = true;
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::assistant_header(colour),
            )?;
        }
        self.pending.push_str(text);
        let complete = drain_lines(&mut self.pending);
        // The live rows held the part of a line still arriving. A finished
        // line replaces them, so they are erased once before the first.
        if self.live_lines > 0 && !complete.is_empty() {
            erase_live_response(terminal, composer, self.live_lines)?;
            self.live_lines = 0;
        }
        for line in complete {
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::assistant_row(colour, &line),
            )?;
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        self.live_lines = redraw_live_response(
            terminal,
            composer,
            colour,
            footer,
            status,
            &self.pending,
            self.live_lines,
        )?;
        Ok(())
    }

    /// Close the round: finish whatever box is open and settle the last line.
    fn close(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        if self.live_lines > 0 {
            erase_live_response(terminal, composer, self.live_lines)?;
            self.live_lines = 0;
        }
        if self.pending.trim().is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.pending);
        stream_row(
            terminal,
            composer,
            colour,
            footer,
            status,
            &tui::assistant_row(colour, &line),
        )
    }

    /// Close the reasoning box if it is open, flushing the line it was part
    /// way through.
    fn close_thinking(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<()> {
        if !self.thinking_open {
            return Ok(());
        }
        self.thinking_open = false;
        let width = tui::terminal_width();
        if !self.thinking.trim().is_empty() {
            let line = std::mem::take(&mut self.thinking);
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_row(width, colour, &line),
            )?;
        }
        stream_row(
            terminal,
            composer,
            colour,
            footer,
            status,
            &tui::thinking_box_bottom(width, colour),
        )
    }
}

/// Take the finished lines out of a streaming buffer, leaving whatever part of
/// the next one has arrived.
///
/// One scan for the last break rather than one per line: a buffer is appended
/// to on every delta, and re-scanning it from the front for each line it holds
/// is quadratic in a long answer.
#[cfg(feature = "tui")]
fn drain_lines(buffer: &mut String) -> Vec<String> {
    let Some(last) = buffer.rfind('\n') else {
        return Vec::new();
    };
    let complete: String = buffer.drain(..=last).collect();
    complete.split_inclusive('\n').map(str::to_owned).collect()
}

/// One finished row above the composer, with the status redrawn under it.
#[cfg(feature = "tui")]
fn stream_row(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &str,
    status: &str,
    row: &str,
) -> io::Result<()> {
    let mut frame = composer.clear();
    frame.push_str(row);
    frame.push('\n');
    frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
    write!(terminal, "{frame}").and_then(|()| terminal.flush())
}

/// What the keys pressed while a round streams amount to.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum Typed {
    /// Nothing that changes what is on screen.
    Quiet,
    Redraw,
    Interrupted,
}

/// Take every key waiting, without blocking on the next one.
///
/// A turn is streaming while this runs, so the composer stays live: a
/// follow-up can be queued, the approval mode can change, and the turn can be
/// stopped, all without waiting for the provider to finish.
#[cfg(feature = "tui")]
fn drain_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
    outcome: &mut Turn,
) -> Typed {
    let mut typed = Typed::Quiet;
    while let Ok(byte) = keys.try_recv() {
        let Some(key) = decoder.feed(byte) else {
            continue;
        };
        if key == tui::Key::Interrupt {
            outcome.queued.clear();
            outcome.interrupted = true;
            return Typed::Interrupted;
        }
        match composer.press(key) {
            // Shift+Tab changes authority immediately; it never becomes a
            // model prompt or a queued follow-up.
            tui::Action::CycleMode => {
                cycle_approval_mode(approval);
                typed = Typed::Redraw;
            }
            // Bounded as on the Codex route, so a held Enter cannot grow the
            // queue without limit; past the bound the draft is handed back.
            tui::Action::Submit(line) if !line.trim().is_empty() => {
                if outcome.queued.len() < 16 {
                    outcome.queued.push_back(line);
                } else {
                    composer.restore(line);
                }
            }
            tui::Action::Submit(_) | tui::Action::Redraw => typed = Typed::Redraw,
            tui::Action::Quit => outcome.quit = true,
            tui::Action::None => {}
        }
    }
    typed
}

/// The request one round of a turn sends.
///
/// Built per round rather than captured once: the instructions are discovered
/// by walking the workspace, and an AGENTS.md the turn just edited is the one
/// the next round should read.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn round_request(
    resolved: &provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
) -> io::Result<CanonicalModelRequest> {
    Ok(CanonicalModelRequest {
        model: ModelKey {
            provider: route.provider.clone(),
            model: route.model.clone(),
        },
        system: system_prompt(
            runtime.workspace(),
            config,
            &route.provider,
            &route.model,
            runtime.execution_mode(),
        ),
        messages: conversation.to_vec(),
        tools: runtime.schemas(),
        max_output_tokens: resolved.endpoint.max_output_tokens,
        effort,
        // One turn can take several requests, one per round of tool calls. The
        // round is part of the key, because a retry must repeat its own
        // request rather than collapse into the one before it.
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(format!("{turn}-{round}"))
            .map_err(io::Error::other)?,
    })
}

/// Read the provider's stream on its own thread, as the rows the turn draws.
///
/// The provider's events are turned into rows here rather than at the far end,
/// so the drawing loop waits on one channel and nothing else.
#[cfg(feature = "tui")]
fn spawn_stream(
    provider: Arc<dyn arsy_kernel::provider::ModelProvider>,
    request: CanonicalModelRequest,
) -> std::sync::mpsc::Receiver<Result<Streamed, String>> {
    let (rows, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = match arsy_kernel::provider::stream_with_retry(
            provider.as_ref(),
            &request,
            &mut std::thread::sleep,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = rows.send(Err(error.to_string()));
                return;
            }
        };
        for event in stream {
            let Some(message) = streamed(event) else {
                continue;
            };
            let failed = message.is_err();
            if rows.send(message).is_err() || failed {
                return;
            }
        }
    });
    events
}

/// The row a provider event draws, or `None` for an event the turn does not
/// show.
#[cfg(feature = "tui")]
fn streamed(
    event: Result<ModelEvent, arsy_kernel::provider::ProviderError>,
) -> Option<Result<Streamed, String>> {
    Some(match event {
        Ok(ModelEvent::TextDelta { text }) => Ok(Streamed::Text(text)),
        Ok(ModelEvent::ThinkingDelta { text }) => Ok(Streamed::Thinking(text)),
        Ok(ModelEvent::Usage {
            input_tokens,
            output_tokens,
        }) => Ok(Streamed::Usage {
            input_tokens,
            output_tokens,
        }),
        Ok(ModelEvent::ToolCallCompleted {
            id,
            name,
            arguments,
            ..
        }) => Ok(Streamed::Tool {
            id,
            name,
            arguments,
        }),
        Ok(_) => return None,
        Err(error) => Err(error.to_string()),
    })
}

/// Say what a context trim removed, when it removed anything.
///
/// A transcript that has outgrown the window fails at the provider, so the
/// operator is told what was elided rather than watching the turn shrink
/// invisibly.
#[cfg(feature = "tui")]
fn report_trim(colour: bool, trimmed: &arsy_code::agent::budget::Trimmed) -> io::Result<()> {
    if !trimmed.changed() {
        return Ok(());
    }
    let mut terminal = io::stdout();
    writeln!(
        terminal,
        "{}",
        tui::tool_result_row(
            colour,
            "context",
            true,
            &format!(
                "elided {} tool result(s) and compacted {} earlier message(s) to stay \
                 within {} tokens",
                trimmed.elided, trimmed.summarized, trimmed.after
            )
        )
    )?;
    terminal.flush()
}

/// The call being answered.
#[cfg(feature = "tui")]
struct Call<'a> {
    name: &'a str,
    arguments: &'a Value,
    /// Identifies the effect, so an identical later call can be answered from
    /// this one's result.
    fingerprint: String,
}

/// What answering a call is allowed to touch.
#[cfg(feature = "tui")]
struct Answering<'a> {
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    approval: &'a approval::ApprovalCell,
    completed: &'a mut std::collections::HashMap<String, String>,
    interrupted: &'a mut bool,
    hooks: Option<&'a arsy_code::hook::HookEngine>,
}

/// Run one call and turn what happened into the result the provider is sent.
#[cfg(feature = "tui")]
fn run_call(
    runtime: &arsy_code::agent::ToolRuntime,
    terminal: &mut io::Stdout,
    colour: bool,
    summary: &str,
    call: Call<'_>,
    answering: Answering<'_>,
) -> io::Result<(String, bool)> {
    // A new effect can invalidate an earlier read or command result, so only
    // reuse calls until the next effectful call.
    if !runtime.is_observational(call.name, call.arguments) {
        answering.completed.clear();
    }
    match execute_call(
        runtime,
        terminal,
        colour,
        call.name,
        call.arguments,
        summary,
        answering.keys,
        answering.decoder,
        answering.approval,
        answering.hooks,
    )? {
        Executed::Answered(mut result) => {
            if !result.changed_files.is_empty() {
                result.output.push_str("\nChanged files:\n");
                for path in &result.changed_files {
                    result.output.push_str(&format!("  • {path}\n"));
                }
            }
            if result.success {
                answering
                    .completed
                    .insert(call.fingerprint, result.output.clone());
            }
            Ok((result.output, !result.success))
        }
        Executed::Stopped => {
            *answering.interrupted = true;
            writeln!(terminal, "{}", tui::interrupted_row(colour))?;
            Ok(("The operator stopped the turn.".to_owned(), true))
        }
    }
}

/// What happened to one tool call.
#[cfg(feature = "tui")]
enum Executed {
    Answered(arsy_code::agent::ToolResult),
    Stopped,
}

/// Decide, confirm if the decision says to, and run.
///
/// Policy is asked first, so the operator is only interrupted for calls that
/// actually need a human: a read policy already allows runs without a prompt,
/// and a call policy denies is refused without one. That is the difference
/// between an approval and a habit — an operator asked to confirm every read
/// stops reading the prompts.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn execute_call(
    runtime: &arsy_code::agent::ToolRuntime,
    terminal: &mut io::Stdout,
    colour: bool,
    name: &str,
    arguments: &Value,
    summary: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    approval: &approval::ApprovalCell,
    hooks: Option<&arsy_code::hook::HookEngine>,
) -> io::Result<Executed> {
    let started = std::time::Instant::now();
    let mut notes = Vec::new();
    let (arguments, injected) =
        match hooks.map(|hooks| hook_before_call(hooks, name, arguments, &mut notes)) {
            None => (arguments.clone(), Vec::new()),
            Some(hooked) => {
                let asking = Asking {
                    name,
                    arguments,
                    summary,
                    keys,
                    decoder: &mut *decoder,
                    approval,
                };
                match hooked_arguments(terminal, colour, hooked, asking, &notes)? {
                    Ok(run) => run,
                    Err(executed) => return Ok(executed),
                }
            }
        };
    let arguments = &arguments;
    let request = match runtime.prepare(name, arguments) {
        Ok(request) => request,
        Err(failure) => return Ok(Executed::Answered(*failure)),
    };
    let authorization = runtime.authorize(&request);
    let (grants, approval_note) = match authorize(
        terminal,
        colour,
        Asking {
            name,
            arguments,
            summary,
            keys,
            decoder,
            approval,
        },
        authorization,
    )? {
        Granted::Run { grants, note } => (grants, note),
        Granted::Refused(result) => return Ok(Executed::Answered(*result)),
        Granted::Stopped => return Ok(Executed::Stopped),
    };
    let (mut result, cancelled) = dispatch_tool_live(
        terminal, colour, runtime, name, &request, &grants, started, summary, keys, decoder,
    )?;
    if cancelled {
        return Ok(Executed::Stopped);
    }
    if let Some(note) = approval_note {
        result.output = format!("{}\nOperator note: {note}", result.output);
    }
    if let Some(hooks) = hooks {
        notes.clear();
        hook_after_call(hooks, name, &injected, &mut result, &mut notes);
        hook_notes(terminal, colour, &notes)?;
    }
    Ok(Executed::Answered(result))
}

/// The arguments a call runs with and what hooks injected for the model.
#[cfg(feature = "tui")]
type HookedRun = (Value, Vec<(String, String)>);

/// What `before_operation` leaves the interactive turn to run.
///
/// Unlike a scripted run, this surface has an operator, so a hook that wants
/// one asked gets the approval dialog rather than a refusal.
#[cfg(feature = "tui")]
fn hooked_arguments(
    terminal: &mut io::Stdout,
    colour: bool,
    hooked: HookedCall,
    asking: Asking<'_>,
    notes: &[String],
) -> io::Result<Result<HookedRun, Executed>> {
    hook_notes(terminal, colour, notes)?;
    let (arguments, injected, reason) = match hooked {
        HookedCall::Refused(reason) => {
            return Ok(Err(Executed::Answered(hook_refused(asking.name, reason))))
        }
        HookedCall::Run {
            arguments,
            injected,
            approval,
        } => (arguments, injected, approval),
    };
    let Some(reason) = reason else {
        return Ok(Ok((arguments, injected)));
    };
    let answer = confirm_tool(
        terminal,
        colour,
        asking.name,
        asking.summary,
        &format!("a hook asks for approval: {reason}"),
        format_tool_preview(asking.name, &arguments),
        asking.keys,
        asking.decoder,
        asking.approval,
    )?;
    Ok(match answer {
        Answer::Yes { .. } => Ok((arguments, injected)),
        Answer::No { .. } => Err(Executed::Answered(hook_refused(
            asking.name,
            "The operator declined the call a hook asked about.".to_owned(),
        ))),
        Answer::Stop => Err(Executed::Stopped),
    })
}

/// What the hooks said about a call, as dim rows above it.
#[cfg(feature = "tui")]
fn hook_notes(terminal: &mut io::Stdout, colour: bool, notes: &[String]) -> io::Result<()> {
    notes
        .iter()
        .try_for_each(|note| writeln!(terminal, "{}", tui::hook_note_row(colour, note)))
}

/// What deciding a call needs in order to ask about it.
#[cfg(feature = "tui")]
struct Asking<'a> {
    name: &'a str,
    arguments: &'a Value,
    summary: &'a str,
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    approval: &'a approval::ApprovalCell,
}

/// What authorizing a call decided.
#[cfg(feature = "tui")]
enum Granted {
    Run {
        grants: Vec<arsy_kernel::capability::CapabilityGrant>,
        /// What the operator said when they approved it, if anything.
        note: Option<String>,
    },
    /// The call does not run, and this is what the provider is told.
    Refused(Box<arsy_code::agent::ToolResult>),
    Stopped,
}

/// Turn an authorization into grants, asking the operator when the mode says
/// to ask.
///
/// Separate from running the call: what a call is allowed to do is decided
/// before anything happens, and reading that decision should not mean reading
/// the execution as well.
#[cfg(feature = "tui")]
fn authorize(
    terminal: &mut io::Stdout,
    colour: bool,
    asking: Asking<'_>,
    authorization: arsy_code::agent::Authorization,
) -> io::Result<Granted> {
    use arsy_code::agent::Authorization;

    let name = asking.name;
    let requested = match &authorization {
        Authorization::Allowed(grants) => {
            return Ok(Granted::Run {
                grants: grants.clone(),
                note: None,
            })
        }
        Authorization::Denied(reason) => return Ok(refused(name, reason.clone())),
        Authorization::NeedsApproval { .. } => authorization.requested(),
    };
    match approval::decide(asking.approval.get(), name) {
        approval::Decision::Approve => Ok(granted(authorization, name, None)),
        approval::Decision::Refuse => Ok(refused(
            name,
            format!(
                "the current approval mode ({}) refuses this call without asking: {}",
                asking.approval.get().label(),
                requested
            ),
        )),
        approval::Decision::Ask => {
            let preview = format_tool_preview(name, asking.arguments);
            match confirm_tool(
                terminal,
                colour,
                name,
                asking.summary,
                &requested,
                preview,
                asking.keys,
                asking.decoder,
                asking.approval,
            )? {
                Answer::Yes { note } => Ok(granted(authorization, name, note)),
                Answer::No { note } => Ok(refused(
                    name,
                    note.map_or_else(
                        || "The operator declined to run this call.".to_owned(),
                        |note| format!("The operator declined to run this call. Feedback: {note}"),
                    ),
                )),
                Answer::Stop => Ok(Granted::Stopped),
            }
        }
    }
}

/// Turn an approved authorization into the grants the call runs under.
#[cfg(feature = "tui")]
fn granted(
    authorization: arsy_code::agent::Authorization,
    name: &str,
    note: Option<String>,
) -> Granted {
    match authorization.approve() {
        Ok(grants) => Granted::Run { grants, note },
        Err(error) => refused(
            name,
            format!("the approval could not be turned into a grant: {error}"),
        ),
    }
}

/// The answer a call that will not run sends back to the provider.
#[cfg(feature = "tui")]
fn refused(name: &str, reason: String) -> Granted {
    Granted::Refused(Box::new(arsy_code::agent::ToolResult::refused(
        name, reason,
    )))
}

fn write_unwrapped_lines(terminal: &mut impl Write, lines: &[String]) -> io::Result<()> {
    write!(terminal, "{}", tui::DISABLE_AUTOWRAP)?;
    for line in lines {
        writeln!(terminal, "{line}")?;
    }
    write!(terminal, "{}", tui::ENABLE_AUTOWRAP)
}

#[cfg(feature = "tui")]
// Every argument is one the live view needs and none of them group into a
// meaningful type: the terminal, the call, and the keyboard are three unrelated
// things this function happens to hold at once.
#[allow(clippy::too_many_arguments)]
fn dispatch_tool_live(
    terminal: &mut io::Stdout,
    colour: bool,
    runtime: &arsy_code::agent::ToolRuntime,
    name: &str,
    request: &arsy_kernel::operation::OperationRequest,
    grants: &[arsy_kernel::capability::CapabilityGrant],
    started: std::time::Instant,
    summary: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> io::Result<(arsy_code::agent::ToolResult, bool)> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let (output_sender, output_receiver) = std::sync::mpsc::channel();
    let output_sink: arsy_kernel::operation::OutputSink = std::sync::Arc::new(move |chunk| {
        let _ = output_sender.send(chunk);
    });
    runtime.set_output_sink(Some(output_sink));
    let worker_runtime = runtime.clone();
    let name = name.to_owned();
    let worker_name = name.clone();
    let request = request.clone();
    let operation_id = request.id;
    let worker_request = request.clone();
    let grants = grants.to_vec();
    std::thread::spawn(move || {
        let result = worker_runtime.dispatch(&worker_name, &worker_request, &grants, started);
        let _ = sender.send(result);
    });

    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let mut frame = 0usize;
    let elapsed = std::time::Instant::now();
    let mut cancelled = false;
    let mut expanded = false;
    let mut live_output = String::new();
    let initial_state = tui::RunningToolState {
        name: name.as_str(),
        summary,
        frame: FRAMES[0],
        elapsed_ms: 0,
        live_output: "",
        expanded,
    };
    let initial = tui::tool_running_box(tui::terminal_width(), colour, &initial_state);
    write_unwrapped_lines(terminal, &initial)?;
    terminal.flush()?;
    let mut last_rendered_lines = initial.len();
    loop {
        if absorb_live_keys(
            keys,
            decoder,
            terminal,
            LiveCall {
                name: &name,
                cancellable: request.kind.to_string() == "process.exec",
                operation_id,
            },
            &mut last_rendered_lines,
            &mut expanded,
        )? {
            cancelled = true;
        }
        match receiver.recv_timeout(std::time::Duration::from_millis(80)) {
            Ok(result) => {
                runtime.set_output_sink(None);
                if last_rendered_lines > 0 {
                    write!(terminal, "\x1b[{}A\r\x1b[J", last_rendered_lines)?;
                    terminal.flush()?;
                }
                return Ok((result, cancelled));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                absorb_output(&output_receiver, &mut live_output);
                frame = frame.wrapping_add(1);
                let state = tui::RunningToolState {
                    name: name.as_str(),
                    summary,
                    frame: FRAMES[frame % FRAMES.len()],
                    elapsed_ms: elapsed.elapsed().as_millis(),
                    live_output: &live_output,
                    expanded,
                };
                let status_lines = tui::tool_running_box(tui::terminal_width(), colour, &state);
                if last_rendered_lines > 0 {
                    write!(terminal, "\x1b[{}A\r\x1b[J", last_rendered_lines)?;
                }
                write_unwrapped_lines(terminal, &status_lines)?;
                terminal.flush()?;
                last_rendered_lines = status_lines.len();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("tool worker disconnected"));
            }
        }
    }
}

#[cfg(feature = "tui")]
fn format_tool_preview(name: &str, arguments: &Value) -> Option<String> {
    match name {
        "apply_patch" | "fs.edit" | "edit" => arguments
            .get("input")
            .or_else(|| arguments.get("patch"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        "fs.write" | "write" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("file");
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            let preview: Vec<String> = content.lines().take(12).map(|l| format!("+{l}")).collect();
            let mut text = format!("--- /dev/null\n+++ {path}\n{}", preview.join("\n"));
            if content.lines().count() > 12 {
                text.push_str(&format!(
                    "\n… ({} lines omitted)",
                    content.lines().count() - 12
                ));
            }
            Some(text)
        }
        "bash" | "shell.execute" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|cmd| format!("$ {cmd}")),
        _ => None,
    }
}

/// Ask the operator whether one tool call may run.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn confirm_tool(
    terminal: &mut io::Stdout,
    colour: bool,
    name: &str,
    summary: &str,
    reason: &str,
    diff_preview: Option<String>,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    approval: &approval::ApprovalCell,
) -> io::Result<Answer> {
    let mut dialog = tui::AskDialogState::for_approval(name, summary, reason, diff_preview);
    let width = tui::terminal_width();
    let mut rendered_lines = dialog.render(width, colour).lines().count();
    writeln!(terminal, "{}", dialog.render(width, colour))?;
    terminal.flush()?;
    approval.open();
    loop {
        match keys.recv() {
            Ok(byte) => match decoder.feed(byte) {
                Some(key) => {
                    if let Some(result) = dialog.handle_key(key) {
                        write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
                        terminal.flush()?;
                        match result {
                            tui::AskDialogResult::Approve { note } => {
                                return Ok(Answer::Yes { note })
                            }
                            tui::AskDialogResult::AlwaysApprove { note } => {
                                approval.set(approval::ApprovalMode::Auto);
                                return Ok(Answer::Yes { note });
                            }
                            tui::AskDialogResult::Deny { note } => return Ok(Answer::No { note }),
                            tui::AskDialogResult::CycleMode => continue,
                            tui::AskDialogResult::Cancel => return Ok(Answer::Stop),
                        }
                    } else {
                        let frame = dialog.render(width, colour);
                        write!(terminal, "\x1b[{}A\r\x1b[J{}\n", rendered_lines, frame)?;
                        terminal.flush()?;
                        rendered_lines = frame.lines().count();
                    }
                }
                None => continue,
            },
            Err(_) => return Ok(Answer::Stop),
        }
    }
}

#[cfg(feature = "tui")]
fn confirm_plan(
    terminal: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    preview: &str,
) -> io::Result<tui::AskDialogResult> {
    let mut dialog = tui::AskDialogState::for_plan(preview.to_owned());
    dialog.set_preview_height(tui::terminal_rows().saturating_sub(16));
    let width = tui::terminal_width();
    let mut frame = dialog.render(width, colour);
    let mut rendered_lines = frame.lines().count();
    writeln!(terminal, "{frame}")?;
    terminal.flush()?;
    loop {
        let Ok(byte) = keys.recv() else {
            return Ok(tui::AskDialogResult::Cancel);
        };
        let Some(key) = decoder.feed(byte) else {
            continue;
        };
        if let Some(result) = dialog.handle_key(key) {
            write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
            terminal.flush()?;
            return Ok(result);
        }
        dialog.set_preview_height(tui::terminal_rows().saturating_sub(16));
        frame = dialog.render(width, colour);
        write!(terminal, "\x1b[{}A\r\x1b[J{}\n", rendered_lines, frame)?;
        terminal.flush()?;
        rendered_lines = frame.lines().count();
    }
}

#[cfg(feature = "tui")]
fn redraw_live_response(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &str,
    status: &str,
    text: &str,
    prev_lines: usize,
) -> io::Result<usize> {
    let mut frame = composer.clear();
    for _ in 0..prev_lines {
        frame.push_str("\x1b[1A\r\x1b[K");
    }
    frame.push_str(&tui::assistant_row(colour, text));
    frame.push('\n');
    let width = tui::terminal_width();
    frame.push_str(&composer.render_turn(width, colour, status, footer));
    write!(terminal, "{frame}")?;
    terminal.flush()?;
    let text_len = unicode_width::UnicodeWidthStr::width(text);
    let lines = text_len.checked_div(width).map_or(1, |div| div + 1);
    Ok(lines)
}

#[cfg(feature = "tui")]
fn erase_live_response(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    lines: usize,
) -> io::Result<()> {
    let mut frame = composer.clear();
    for _ in 0..lines.max(1) {
        frame.push_str("\x1b[1A\r\x1b[K");
    }
    write!(terminal, "{frame}")?;
    terminal.flush()
}

/// Stream one round of a turn from a configured provider, keeping the composer
/// alive.
///
/// The stream runs on its own thread for the same reason the Codex reader
/// does: the main loop has to keep watching the key stream, which is what lets
/// Esc or Ctrl-C stop a turn and keeps the composer typeable meanwhile. The
/// thread is detached rather than joined, so an interrupt never waits on a
/// stalled socket; dropping the receiver is what stops it, because the next
/// send fails and the stream is dropped with the thread.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn native_status(
    resolved: &provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
) -> io::Result<Turn> {
    let request = round_request(
        resolved,
        config,
        runtime,
        conversation,
        route,
        effort,
        turn,
        round,
    )?;
    let events = spawn_stream(Arc::clone(&resolved.provider), request);

    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    // Deltas arrive token by token; a row is emitted per line so scrollback
    // reads like the Codex projection rather than one row per token.
    //
    // Thinking and the answer hold separate buffers, and each thinking section
    // is announced once with its own header row, so the verbose stream reads as
    // distinct parts of the turn rather than one grey blur.
    let mut live = Streaming::default();
    let started = std::time::Instant::now();
    let mut tick = 0usize;
    // A static `Working…` line cannot tell a slow connect from a hang; the
    // status is rebuilt on every timer pass instead of captured once.
    let status_line = |first_event: bool, tick: usize| {
        tui::turn_status(
            colour,
            if first_event {
                tui::TurnPhase::Answering
            } else {
                tui::TurnPhase::Connecting
            },
            started.elapsed(),
            tick,
            0,
        )
    };
    let mut first_event = false;
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                status: &str| {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, &status_line(false, 0))?;
    loop {
        let typed = drain_keys(keys, decoder, composer, approval, &mut outcome);
        if typed == Typed::Interrupted {
            draw(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                &status_line(first_event, tick),
            )?;
            return finish(terminal, composer, outcome);
        }
        // The status is alive: the spinner advances and the seconds climb even
        // while the provider sends nothing, so a silent turn never reads as a
        // frozen one.
        tick = tick.wrapping_add(1);
        if typed == Typed::Redraw {
            draw(
                &mut terminal,
                composer,
                None,
                &status_line(first_event, tick),
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(Ok(Streamed::Thinking(text))) => {
                let status = status_line(first_event, tick);
                live.reason(&mut terminal, composer, colour, footer, &status, &text)?;
                first_event = true;
            }
            Ok(Ok(Streamed::Text(text))) => {
                outcome.response.push_str(&text);
                let status = status_line(first_event, tick);
                live.answer(&mut terminal, composer, colour, footer, &status, &text)?;
                first_event = true;
            }
            Ok(Ok(Streamed::Usage {
                input_tokens,
                output_tokens,
            })) => {
                outcome.usage =
                    json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
            Ok(Ok(Streamed::Tool {
                id,
                name,
                arguments,
            })) => {
                outcome.calls.push((id, name, arguments));
                first_event = true;
            }
            Ok(Err(failure)) => {
                outcome.failure = Some(failure);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if decoder.flush_escape() == Some(tui::Key::Interrupt) {
                    outcome.interrupted = true;
                    if live.live_lines > 0 {
                        erase_live_response(&mut terminal, composer, live.live_lines)?;
                    }
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::interrupted_row(colour)),
                        &status_line(first_event, tick),
                    )?;
                    return finish(terminal, composer, outcome);
                }
                // Repaint the live status on every idle pass.
                draw(
                    &mut terminal,
                    composer,
                    None,
                    &status_line(first_event, tick),
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    live.close(
        &mut terminal,
        composer,
        colour,
        footer,
        &status_line(first_event, tick),
    )?;
    finish(terminal, composer, outcome)
}

/// One streamed fact from a provider, as the terminal needs it.
#[cfg(feature = "tui")]
enum Streamed {
    Text(String),
    Thinking(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A complete tool call. The host runs it after the stream ends, so a turn
    /// is never edited from under a model that is still writing.
    Tool {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Tear the composer down so the next thing printed starts on its own line.
#[cfg(feature = "tui")]
fn finish(mut terminal: io::Stdout, composer: &mut tui::Composer, turn: Turn) -> io::Result<Turn> {
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    Ok(turn)
}

#[cfg(feature = "tui")]
/// Run the task through the logged-in Codex CLI and project its JSONL event
/// stream as ARSY rows, so the terminal shows one interface, not two.
#[allow(clippy::too_many_arguments)]
fn external_status(
    workspace: &Path,
    task: &str,
    route: &tui::ModelRoute,
    approval: &approval::ApprovalCell,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let mode = approval.get();
    let mut command = std::process::Command::new("codex");
    command.args([
        "exec",
        "--json",
        "--ephemeral",
        "--sandbox",
        if matches!(
            mode,
            approval::ApprovalMode::AcceptEdits
                | approval::ApprovalMode::Auto
                | approval::ApprovalMode::BypassPermissions
        ) {
            "workspace-write"
        } else {
            "read-only"
        },
        "--cd",
    ]);
    command.arg(workspace);
    if route.model != "default" {
        command.args(["--model", &route.model]);
    }
    command.arg("-");
    command.current_dir(workspace);
    let task = if mode == approval::ApprovalMode::Plan {
        format!(
            "{}\n\n{}",
            arsy_code::agent::instructions::PLAN_MODE_INSTRUCTIONS,
            task
        )
    } else {
        task.to_owned()
    };
    drive_provider(
        command, &task, route, approval, colour, footer, keys, decoder, composer, redactor,
    )
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn drive_provider(
    command: std::process::Command,
    task: &str,
    route: &tui::ModelRoute,
    approval: &approval::ApprovalCell,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let (mut child, errors, input, events) = spawn_provider(command, task)?;
    let started = std::time::Instant::now();
    let mut cancelling = None;
    let mut stream_closed = false;
    let mut finished = None;
    let mut last_event = std::time::Instant::now();
    let mut stopped_early = false;
    let mut last_row = None;
    let mut last_key = std::time::Instant::now();
    let mut exited = None;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut outcome = Turn::default();
    let mut successful_git_commands = std::collections::HashSet::new();
    let mut terminal = io::stdout();
    composer.set_height(tui::terminal_rows());
    let painter = Painter {
        colour,
        footer,
        width: std::cell::Cell::new(tui::terminal_width()),
        started,
    };
    painter.row(&mut terminal, composer, None, false, 0, 0)?;
    let mut refreshed = std::time::Instant::now();
    loop {
        let tick = (started.elapsed().as_millis() / 100) as usize;
        if let Ok(result) = input.try_recv() {
            if !outcome.interrupted {
                result?;
            }
        }
        match lifecycle(
            &mut child,
            Clocks {
                started,
                status: &mut status,
                exited: &mut exited,
                finished: &mut finished,
                last_event,
                cancelling,
                stopped_early: &mut stopped_early,
            },
        )? {
            Pass::Stop => break,
            Pass::Go => {}
        }
        let typed = provider_keys(
            Keyboard {
                keys,
                decoder,
                composer,
                approval,
            },
            &mut terminal,
            &painter,
            &mut child,
            Turning {
                outcome: &mut outcome,
                cancelling: &mut cancelling,
                last_key: &mut last_key,
            },
            colour,
            tick,
        )?;
        if last_key.elapsed() >= std::time::Duration::from_millis(40)
            && decoder.flush_escape() == Some(tui::Key::Interrupt)
            && stop_turn(&mut outcome, &mut child, &mut cancelling)
        {
            painter.row(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                true,
                0,
                tick,
            )?;
        }
        let resize_tick = remeasure(&painter, composer, &mut refreshed);
        if typed || resize_tick {
            painter.row(
                &mut terminal,
                composer,
                None,
                cancelling.is_some(),
                outcome.queued.len(),
                tick,
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(_) if outcome.interrupted => {}
            Ok(line) => {
                last_event = std::time::Instant::now();
                show_event(
                    &line?,
                    redactor,
                    &mut child,
                    &painter,
                    &mut terminal,
                    composer,
                    Streamlined {
                        outcome: &mut outcome,
                        finished: &mut finished,
                        stopped_early: &mut stopped_early,
                        seen_git: &mut successful_git_commands,
                        last_row: &mut last_row,
                    },
                    colour,
                    tick,
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Repaint so the spinner and clock stay alive while the
                // provider is quiet, not just when an event or a key arrives.
                painter.row(
                    &mut terminal,
                    composer,
                    None,
                    cancelling.is_some(),
                    outcome.queued.len(),
                    tick,
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => stream_closed = true,
        }
        if stream_closed {
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    if !outcome.interrupted && finished.is_none() {
        return Err(silent_provider(&errors, redactor)?);
    }
    if !outcome.interrupted {
        outcome.failure = verdict(&mut child, route, status, stopped_early, &outcome)?;
    }
    Ok(outcome)
}

/// What one interactive turn left behind, whichever route ran it.
#[cfg(feature = "tui")]
#[derive(Default)]
struct Turn {
    /// `None` when the turn succeeded; otherwise why it did not.
    failure: Option<String>,
    /// The full text of the model's answer, kept so follow-up turns in the
    /// same session know what the model said.
    response: String,
    /// Extra facts to record on a completed turn, such as token usage.
    usage: Value,
    interrupted: bool,
    provider_failed: bool,
    /// A line submitted while this turn was still running.
    queued: std::collections::VecDeque<String>,
    quit: bool,
    /// Tool calls the model made and the host has not run yet: id, name, and
    /// arguments. Only complete calls land here, so a truncated stream cannot
    /// leave a half-parsed call to execute.
    calls: Vec<(String, String, Value)>,
}
#[cfg(feature = "tui")]
/// Record and report a turn the provider did not complete.
///
/// The code and the reason follow the route, because "the CLI failed" is not
/// something to tell an operator whose turn went straight to an endpoint, and
/// the recorded reason is what a later audit reads.
fn fail_turn(
    service: &AgentService,
    actor: Principal,
    turn: arsy_kernel::domain::TurnId,
    session: SessionId,
    route: &tui::ModelRoute,
    message: String,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let (code, reason, remediation) = if route.is_codex() {
        (
            "ARSY-PRV-1002",
            "provider_cli",
            "verify the selected CLI login and model, then retry",
        )
    } else {
        (
            ARSY_PRV_1000,
            "provider",
            "check the provider endpoint, credential, and model in `arsy config explain`",
        )
    };
    let diagnostic = Diagnostic::error(code, message, remediation);
    service
        .fail_turn(actor, turn, reason, diagnostic.message.clone())
        .map_err(storage_failed)?;
    emitter.diagnostic(&diagnostic);
    turn_record(
        emitter,
        json!({
            "session": session.to_string(),
            "turn": turn.to_string(),
            "status": "failed",
        }),
    );
    Ok(diagnostic.exit_code())
}

/// Emit the machine record for one interactive turn.
///
/// The record is the audit trail machines read, so `--output json` and `ci`
/// keep it. Printing it after every reply in the interactive terminal only
/// buries the reply, and the same evidence is already durable in the session
/// store, reachable with `arsy resume`.
#[cfg(feature = "tui")]
fn turn_record(emitter: &mut Emitter, payload: Value) {
    if emitter.output != Output::Human {
        emitter.result(payload);
    }
}

fn run(
    invocation: &Invocation,
    task: &str,
    image: Option<&Path>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let task = if task == "-" {
        read_stdin()?
    } else {
        task.to_owned()
    };
    let goal = prepare_task(invocation, &task, emitter)?;
    // Read before the session is opened: a path that is not an image, or is
    // too large, is the operator's mistake and should not cost a recorded turn.
    let attached = image.map(read_image).transpose()?;
    let mut execution = match TaskRun::open(invocation, None) {
        Ok(execution) => execution,
        Err(diagnostic) => return Ok(unusable(diagnostic, emitter)),
    };
    emitter.session = Some(execution.session);
    let task = execution.enqueue(&goal)?;
    execution.attached = attached;
    execution.execute(task, Value::Null, emitter)
}

/// The most one attached image may be.
///
/// Large enough for a full-resolution screenshot, small enough that a
/// mis-typed path to a video does not become a request nobody can send. The
/// encoding grows it by a third, and every endpoint in this family refuses
/// well below that.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Read an image the operator attached, as canonical model content.
fn read_image(path: &Path) -> Result<ModelContent, Diagnostic> {
    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        other => {
            return Err(usage(format!(
                "--image accepts png, jpeg, gif, or webp, not `{}`",
                other.unwrap_or("a file with no extension")
            )))
        }
    };
    let size = std::fs::metadata(path)
        .map_err(|error| usage(format!("--image {}: {error}", path.display())))?
        .len();
    if size > MAX_IMAGE_BYTES {
        return Err(usage(format!(
            "--image {} is {size} bytes; the limit is {MAX_IMAGE_BYTES}",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| usage(format!("--image {}: {error}", path.display())))?;
    Ok(ModelContent::Image {
        media_type: media_type.to_owned(),
        data: arsy_kernel::provider::base64(&bytes),
    })
}

/// A misconfiguration is the operator's to fix, not a failed turn in their
/// session history, so nothing is recorded before it is reported.
fn unusable(mut diagnostic: Diagnostic, emitter: &mut Emitter) -> i32 {
    if diagnostic.code == ARSY_PRV_1000 {
        diagnostic.remediation = format!(
            "{}; or use the interactive TUI with a logged-in Codex CLI",
            diagnostic.remediation
        );
    }
    emitter.diagnostic(&diagnostic);
    diagnostic.exit_code()
}

/// How long a task's lease runs before another process may take it over.
///
/// A turn still in flight has not lost its lease; a process that died halfway
/// has, and telling those apart is the whole job of `arsy resume`.
const TASK_LEASE_MS: u64 = 30 * 60 * 1000;

/// What one task may spend before it is stopped rather than continued.
///
/// Wall time matches the lease, because a task that outlives its lease is one
/// another process may already have taken. Tokens are several turns' worth of
/// transcript: the point is to stop a runaway, not to second-guess a long task.
const TASK_BUDGET: Budget = Budget {
    tokens: CONTEXT_BUDGET_TOKENS as u64 * 4,
    cost_micros: u64::MAX,
    wall_ms: TASK_LEASE_MS,
};

/// One session's execution: the store, the provider it dispatches to, and the
/// durable graph of tasks it is working through.
///
/// `arsy run` and `arsy resume` differ only in where the task comes from — a
/// new one, or one a dead process left behind — so everything after that point
/// is this, and a resumed task cannot drift from a fresh one by being executed
/// somewhere else.
struct TaskRun<'a> {
    invocation: &'a Invocation,
    root: PathBuf,
    config: Config,
    resolved: provider::Resolved,
    model: String,
    service: AgentService,
    actor: Principal,
    session: SessionId,
    graph: TaskGraph,
    /// This process's identity as a task holder, so an expired lease can be
    /// told from one this process still holds.
    agent: AgentId,
    /// An image `--image` attached to the prompt, sent with the first message.
    attached: Option<ModelContent>,
}

impl<'a> TaskRun<'a> {
    /// Resolve everything a turn needs, then attach to the session.
    ///
    /// Configuration is resolved first and on its own: a provider that cannot
    /// be reached is a diagnostic before anything is recorded.
    fn open(invocation: &'a Invocation, session: Option<SessionId>) -> Result<Self, Diagnostic> {
        let root = workspace_root(&invocation.workspace)?;
        let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
        let config = load_config(&root, &working, invocation.config.as_deref())?;
        let resolved = provider::resolve(&config, invocation.provider.as_deref())?;
        let model = selected_model(&config, &resolved.endpoint, invocation.model.as_deref())?;

        let store = open_store(&root)?;
        let session = session.unwrap_or_default();
        let actor = actor();
        let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
            .map_err(storage_failed)?;
        let graph = TaskGraph::new(store, session, actor.clone()).map_err(graph_failed)?;
        Ok(Self {
            invocation,
            root,
            config,
            resolved,
            model,
            service,
            actor,
            session,
            graph,
            agent: AgentId::new(),
            attached: None,
        })
    }

    /// Record a new task in the graph and take it.
    fn enqueue(&mut self, goal: &str) -> Result<TaskId, Diagnostic> {
        let id = TaskId::new();
        self.graph
            .add(TaskNode {
                id,
                goal: goal.to_owned(),
                dependencies: Vec::new(),
                assignee: Some(self.agent),
                required_output: "an answer to the task".to_owned(),
                // One process, one working tree: a scripted run edits the
                // workspace it was pointed at.
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget: TASK_BUDGET,
                // Authority comes from policy at dispatch, not from the node:
                // a grant recorded here would be a second, stale answer to the
                // question `RuleSet::evaluate` already answers per call.
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
            })
            .map_err(graph_failed)?;
        Ok(id)
    }

    /// Run one task to a terminal state, recording what it spent on the way.
    ///
    /// `context` is folded into the result: a resumed task reports what its
    /// recovery found in the same record as its outcome, so one invocation
    /// still produces exactly one result.
    fn execute(
        &mut self,
        task: TaskId,
        context: Value,
        emitter: &mut Emitter,
    ) -> Result<i32, Diagnostic> {
        self.graph.ready().map_err(graph_failed)?;
        self.graph
            .lease(task, self.agent, unix_time_ms() + TASK_LEASE_MS)
            .map_err(graph_failed)?;
        let goal = self
            .graph
            .node(task)
            .map(|node| node.goal.clone())
            .ok_or_else(|| storage_failed("the task disappeared from its own graph"))?;

        // No operator is present, so nothing can be confirmed mid-run: the risk
        // context says so, and a call that needs an approval is refused by
        // policy rather than waiting on a keyboard that is not there.
        let agent = agent_runtime(
            &self.root,
            &self.config,
            false,
            &task.to_string(),
            Some(self.session),
            None,
            emitter,
        )?;
        // A supervisor exists only when policy actually delegates something,
        // so a workspace that grants nothing sees no spawn tool rather than one
        // that always refuses.
        let supervisor = subagent::Supervisor::new(
            self.root.clone(),
            &self.config,
            &self.resolved,
            self.model.clone(),
            task,
            &agent,
        );
        let delegates = supervisor.can_delegate();
        // Loaded once, before the turn starts: a turn finishes with the hooks
        // it began with, so a file edited mid-run cannot change the rules under
        // an agent already applying them.
        let loaded = hook_engine(&self.root, &self.config);
        let hooks = (!loaded.is_empty()).then_some(&loaded.engine);
        let goal = match turn_boundary(
            hooks,
            arsy_code::hook::LifecycleEvent::BeforeTurn,
            &goal,
            emitter,
        ) {
            Ok(goal) => goal,
            Err(reason) => {
                return Err(Diagnostic::error(
                    "ARSY-HOK-1001",
                    reason,
                    "the hook that refused it is listed by `arsy hook list`",
                ))
            }
        };
        let admission = self.start_turn(&goal)?;
        let request = CanonicalModelRequest {
            model: ModelKey {
                provider: self.resolved.endpoint.id.clone(),
                model: self.model.clone(),
            },
            system: system_prompt(
                &self.root,
                &self.config,
                &self.resolved.endpoint.id,
                &self.model,
                arsy_code::agent::ExecutionMode::Normal,
            ),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: std::iter::once(ModelContent::Text { text: goal })
                    .chain(self.attached.clone())
                    .collect(),
            }],
            tools: {
                let mut tools = agent.schemas();
                if delegates {
                    tools.push(subagent::schema());
                }
                tools
            },
            max_output_tokens: self.resolved.endpoint.max_output_tokens,
            // Reasoning effort is chosen in the TUI with `/effort`. A scripted
            // run takes the request it always took, so a remembered interactive
            // choice cannot quietly change what a pipeline sends.
            effort: None,
            // The turn id, so a retried attempt is provably the same request.
            idempotency_key: IdempotencyKey::new(admission.turn.to_string())
                .map_err(|error| storage_failed(error.to_string()))?,
        };

        let started = Instant::now();
        let mut recorder = telemetry::Recorder::new(&self.config, self.actor.clone())?;
        let mut supervising = delegates.then_some((supervisor, &mut self.graph));
        let outcome = dispatch(
            self.resolved.provider.as_ref(),
            &agent,
            &request,
            &mut recorder,
            &mut supervising,
            hooks,
            self.config.max_parallel_tools(),
            emitter,
        );
        let interventions: Vec<Value> = supervising
            .as_ref()
            .map(|(supervisor, _)| supervisor.interventions().to_vec())
            .unwrap_or_default();
        drop(supervising);
        let stop = match &outcome {
            Ok(_) => "answered".to_owned(),
            Err(error) => format!("provider:{}", error.code()),
        };
        // The turn has ended whatever it ended as, which is what Codex's own
        // `notify` is for. Nothing it returns can change what already happened.
        let _ = turn_boundary(
            hooks,
            arsy_code::hook::LifecycleEvent::AfterTurn,
            &stop,
            emitter,
        );
        let summary = recorder.finish(&stop, &redactor(self.invocation, emitter)?, emitter);
        let mut record = json!({
            "session": self.session.to_string(),
            "task": task.to_string(),
            "turn": admission.turn.to_string(),
            "provider": self.resolved.endpoint.id,
            "model": request.model.model,
            "telemetry": summary,
            "interventions": interventions,
            // The same projection `/plan` and `/todo` draw, so a pipeline can
            // read where the work stands without replaying raw events — and
            // sees exactly what an operator watching the TUI would have seen.
            "plan": progress::plan(&self.root, &task.to_string()),
            "todos": open_store(&self.root)
                .ok()
                .and_then(|store| progress::todos(store as Arc<dyn EventStore>, self.session)),
        });
        merge(&mut record, context);
        let input_tokens = summary_number(&record, "input_tokens");
        let output_tokens = summary_number(&record, "output_tokens");
        // What the turn cost, when configuration says what the model charges.
        // `None` is reported as unknown rather than as zero: a session that
        // claims it spent nothing is worse than one that admits it cannot say.
        let priced = charge(
            Some(&self.resolved.endpoint),
            &self.model,
            input_tokens,
            output_tokens,
        );
        merge(
            &mut record,
            json!({
                "cost_micros": priced,
                "cost_source": if priced.is_some() { "configured" } else { "unknown" },
            }),
        );
        // Recorded whether the turn completed or failed: a turn that died
        // halfway still spent the tokens it spent, and a session's totals are
        // wrong if the failures are missing from them.
        self.service
            .record_usage(
                self.actor.clone(),
                arsy_kernel::projection::UsageTotals {
                    input_tokens,
                    output_tokens,
                    cost_micros: priced,
                },
            )
            .map_err(storage_failed)?;
        // Charged before the task is closed, so an exhausted budget is on the
        // record even when the turn it exhausted answered anyway.
        if let Err(error) = self.graph.consume(
            task,
            Budget {
                tokens: input_tokens + output_tokens,
                cost_micros: priced.unwrap_or(0),
                wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        ) {
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-RET-1000",
                error.to_string(),
                "split the task, or raise what one task may spend",
            ));
        }

        match outcome {
            Ok(usage) => {
                let mut outcome = record.clone();
                merge(&mut outcome, usage);
                self.service
                    .record_transcript(
                        self.actor.clone(),
                        admission.turn,
                        outcome.get("transcript").unwrap_or(&Value::Null),
                    )
                    .map_err(storage_failed)?;
                self.service
                    .complete_turn(self.actor.clone(), admission.turn, &outcome)
                    .map_err(storage_failed)?;
                self.graph
                    .complete(task, outcome.clone())
                    .map_err(graph_failed)?;
                let mut result = outcome;
                merge(&mut result, json!({"status": "completed"}));
                emitter.result(result);
                Ok(0)
            }
            Err(error) => {
                let diagnostic = Diagnostic::error(
                    ARSY_PRV_1000,
                    error.to_string(),
                    "check the provider endpoint, credential, and model in `arsy config explain`",
                );
                // The turn is durable before dispatch, so a failure here stays
                // recoverable through `arsy resume`.
                self.service
                    .fail_turn(
                        self.actor.clone(),
                        admission.turn,
                        error.code(),
                        error.to_string(),
                    )
                    .map_err(storage_failed)?;
                self.graph
                    .fail(
                        task,
                        json!({"code": error.code(), "message": error.to_string()}),
                    )
                    .map_err(graph_failed)?;
                emitter.diagnostic(&diagnostic);
                let mut result = record;
                merge(&mut result, json!({"status": "failed"}));
                emitter.result(result);
                Ok(diagnostic.exit_code())
            }
        }
    }

    fn start_turn(&self, goal: &str) -> Result<arsy_kernel::service::TurnAdmission, Diagnostic> {
        let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session: self.session,
            prompt: goal.to_owned(),
            extensions: Extensions::new(),
        }));
        self.service
            .start_turn(self.actor.clone(), &envelope)
            .map_err(storage_failed)
    }
}

fn graph_failed(error: arsy_kernel::orchestration::GraphError) -> Diagnostic {
    Diagnostic::error(
        "ARSY-STL-1000",
        format!("the task graph refused the change: {error}"),
        "inspect the session with `arsy session show --turns`",
    )
}

/// The transcript budget, in tokens, before the model's own output is reserved.
///
/// Deliberately below the smallest window the supported models offer rather
/// than read from configuration: the cost of being wrong low is a re-read, and
/// the cost of being wrong high is a rejected request in the middle of a turn.
/// A per-model window belongs in `provider.endpoint` when a model that needs a
/// different number actually appears.
const CONTEXT_BUDGET_TOKENS: u32 = 96_000;

/// What one turn's transcript may grow to on this endpoint.
#[cfg(feature = "tui")]
fn context_budget(resolved: &provider::Resolved) -> u32 {
    CONTEXT_BUDGET_TOKENS.saturating_sub(resolved.endpoint.max_output_tokens)
}

/// How many rounds of tool calls one scripted turn may take.
///
/// The same bound the interactive loop uses, for the same reason: a model that
/// answers every result with another call would otherwise spend the run on its
/// own loop.
const MAX_SCRIPTED_TOOL_ROUNDS: usize = 24;

/// Run one scripted turn to completion, executing the tools the model asks for.
///
/// Nobody is at the keyboard, so authority comes from policy alone: a call
/// policy allows runs, and a call that needs an approval is reported to the
/// model as a failed result rather than silently skipped. That is what makes a
/// pipeline's behaviour a property of its configuration instead of a property
/// of who happened to be watching.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    recorder: &mut telemetry::Recorder,
    supervisor: &mut Option<(subagent::Supervisor<'_>, &mut TaskGraph)>,
    hooks: Option<&arsy_code::hook::HookEngine>,
    parallel: usize,
    emitter: &mut Emitter,
) -> Result<Value, ProviderError> {
    let mut request = request.clone();
    let base = request.idempotency_key.as_str().to_owned();
    let budget = CONTEXT_BUDGET_TOKENS.saturating_sub(request.max_output_tokens);
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    for round in 0..MAX_SCRIPTED_TOOL_ROUNDS {
        // Each round is its own request, so a retry repeats that round rather
        // than collapsing into the one before it.
        request.idempotency_key = IdempotencyKey::new(format!("{base}-{round}"))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        emitter.trace(
            "request",
            json!({
                "round": round,
                "provider": request.model.provider,
                "model": request.model.model,
                "idempotency_key": request.idempotency_key.as_str(),
                "messages": request.messages.len(),
                "tools": request.tools.len(),
                "max_output_tokens": request.max_output_tokens,
                "context_budget_tokens": budget,
            }),
        );
        let mut answer = String::new();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        // Each sleep the retry loop asks for is one attempt that failed, which
        // is the only place a retry is observable from outside the provider.
        let mut retries = 0;
        let started = Instant::now();
        let stream = arsy_kernel::provider::stream_with_retry(provider, &request, &mut |delay| {
            retries += 1;
            // The only place a retry is observable from outside the provider,
            // and the question an operator debugging a slow turn is asking.
            emitter.trace(
                "retry",
                json!({"round": round, "attempt": retries, "delay_ms": delay.as_millis() as u64}),
            );
            std::thread::sleep(delay);
        })
        .inspect_err(|error| {
            recorder.model_call(
                &request.model.model,
                started.elapsed(),
                0,
                0,
                retries,
                error.code(),
            );
        })?;
        let (mut round_input, mut round_output) = (0u64, 0u64);
        for event in stream {
            match event? {
                ModelEvent::TextDelta { text } => {
                    emitter.delta(&text);
                    answer.push_str(&text);
                }
                ModelEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                } => {
                    round_input += input;
                    round_output += output;
                    input_tokens += input;
                    output_tokens += output;
                }
                ModelEvent::ToolCallCompleted {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    emitter.trace(
                        "model.tool_call",
                        json!({"round": round, "id": id, "name": name, "arguments": arguments}),
                    );
                    calls.push((id, name, arguments));
                }
                ModelEvent::Completed { .. }
                | ModelEvent::ToolCallStarted { .. }
                | ModelEvent::ToolCallDelta { .. }
                // `arsy run` is a scriptable surface: reasoning is for the
                // operator watching a stream, not for a pipeline's stdout.
                | ModelEvent::ThinkingDelta { .. } => {}
            }
        }
        recorder.model_call(
            &request.model.model,
            started.elapsed(),
            round_input,
            round_output,
            retries,
            "ok",
        );
        emitter.trace(
            "round.finished",
            json!({
                "round": round,
                "input_tokens": round_input,
                "output_tokens": round_output,
                "retries": retries,
                "tool_calls": calls.len(),
                "answer_bytes": answer.len(),
            }),
        );
        if calls.is_empty() {
            emitter.end_deltas();
            if !answer.trim().is_empty() {
                request.messages.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::Text {
                        text: answer.clone(),
                    }],
                });
            }
            let mut usage = token_usage(input_tokens, output_tokens);
            merge(
                &mut usage,
                json!({
                    "response": answer,
                    // Everything after the system prompt's own message: the
                    // turn's exchange, which is what a resume replays.
                    "transcript": transcript::persistable(&request.messages),
                }),
            );
            return Ok(usage);
        }
        arsy_code::agent::budget::fit(&mut request.messages, budget, None);

        // The calls are history now, whatever running them produced: a provider
        // that sent a call and never sees its result rejects the next request.
        let mut content: Vec<ModelContent> = Vec::new();
        if !answer.trim().is_empty() {
            content.push(ModelContent::Text { text: answer });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        request.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });
        // Independent reads run together; anything that writes, runs a
        // command, or spawns a child still runs alone and in order. `hooks`
        // are the exception: a hook engine is one interpreter with its own
        // recursion guard, so a workspace that loads hooks keeps the
        // sequential path rather than racing them.
        let batched = if hooks.is_none() && supervisor.is_none() && parallel > 1 {
            let batch: Vec<(String, Value)> = calls
                .iter()
                .map(|(_, name, arguments)| (name.clone(), arguments.clone()))
                .collect();
            runtime.invoke_batch(&batch, parallel)
        } else {
            Vec::new()
        };
        let results = calls
            .iter()
            .enumerate()
            .map(|(position, (id, name, arguments))| {
                // Spawning is the one call the tool runtime does not own: it
                // adds a node to this session's graph rather than touching the
                // workspace, and the child's own calls go through the runtime
                // under the authority the graph attenuated for it.
                let result = match (batched.get(position), name.as_str(), supervisor.as_mut()) {
                    // Already run, in whatever order the batch chose; the
                    // position is what pairs it back to this call's id.
                    (Some(result), _, _) => result.clone(),
                    (None, "task.spawn", Some((supervisor, graph))) => {
                        supervisor.spawn(arguments, graph, emitter)
                    }
                    _ => invoke_hooked(hooks, runtime, name, arguments, emitter),
                };
                recorder.tool_call(&result);
                emitter.trace(
                    "tool.result",
                    json!({
                        "round": round,
                        "id": id,
                        "name": name,
                        "success": result.success,
                        "duration_ms": result.duration.as_millis() as u64,
                        "output_bytes": result.output.len(),
                        "changed_files": result.changed_files,
                        "artifact": result.artifact.map(|id| id.to_string()),
                    }),
                );
                ModelContent::ToolResult {
                    id: id.clone(),
                    content: result.output,
                    is_error: !result.success,
                }
            })
            .collect();
        request.messages.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
    }
    emitter.end_deltas();
    Err(ProviderError::InvalidRequest(format!(
        "the model asked for tools {MAX_SCRIPTED_TOOL_ROUNDS} times without finishing the turn"
    )))
}

/// Where Claude Code and Codex keep the operator's files.
///
/// A unit test gets none at all, so what it asserts cannot depend on the
/// Claude or Codex setup of the machine it happens to run on.
fn compat_homes() -> arsy_compat::CompatHomes {
    if cfg!(test) {
        arsy_compat::CompatHomes::none()
    } else {
        arsy_compat::CompatHomes::from_env()
    }
}

/// The engine for this workspace, built from the operator's files and the
/// repository's — the latter only where the operator vouched for it.
fn hook_engine(root: &Path, config: &arsy_kernel::config::Config) -> arsy_code::hook::Loaded {
    arsy_code::hook::load(&arsy_code::hook::Discovery {
        homes: compat_homes(),
        arsy_home: std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from),
        claude: config.compat_enabled("claude"),
        codex: config.compat_enabled("codex"),
        root: root.to_path_buf(),
        trusted: config.trusts(root),
        // One means hooks run and nothing they do dispatches again.
        max_depth: 1,
    })
}

/// Dispatch a turn-boundary event, returning the subject the turn should use.
///
/// `before_turn` may rewrite the prompt or refuse the turn outright;
/// `after_turn` only observes, and its refusal would undo nothing.
fn turn_boundary(
    hooks: Option<&arsy_code::hook::HookEngine>,
    event: arsy_code::hook::LifecycleEvent,
    subject: &str,
    emitter: &mut Emitter,
) -> Result<String, String> {
    use arsy_code::hook::Outcome;

    let Some(hooks) = hooks else {
        return Ok(subject.to_owned());
    };
    // The event names the boundary; the prompt is the payload, not the key, so
    // two turns with the same text are still two dispatches.
    let dispatched = match hooks.dispatch(event, event.as_str(), json!({"prompt": subject})) {
        Ok(dispatched) => dispatched,
        Err(error) => {
            let message = format!("a hook on `{}` failed: {error}", event.as_str());
            if event.failure_policy() == arsy_code::hook::FailurePolicy::FailClosed {
                return Err(message);
            }
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-HOK-1000",
                message,
                String::new(),
            ));
            return Ok(subject.to_owned());
        }
    };
    for note in dispatched.diagnostics {
        emitter.diagnostic(&Diagnostic::warning("ARSY-HOK-1000", note, String::new()));
    }
    match dispatched.outcome {
        Outcome::Deny(reason) => return Err(format!("a hook stopped this turn: {reason}")),
        Outcome::RequireApproval(reason) => {
            return Err(format!(
                "a hook asked for the operator's approval before this turn, and this surface \
                 cannot ask for one: {reason}"
            ))
        }
        Outcome::Continue | Outcome::Allow => {}
    }
    let mut prompt = dispatched.payload["prompt"]
        .as_str()
        .unwrap_or(subject)
        .to_owned();
    for (rule, text) in &dispatched.injected {
        prompt.push_str(&format!("\n\n[hook {rule}] {text}"));
    }
    Ok(prompt)
}

/// What `before_operation` decided about one tool call.
enum HookedCall {
    /// Run it with these arguments, which a hook may have rewritten.
    Run {
        arguments: Value,
        /// Context a hook asked the model to see, by the rule that asked.
        injected: Vec<(String, String)>,
        /// A hook's reason for wanting the operator asked first.
        approval: Option<String>,
    },
    /// Do not run it; tell the model why.
    Refused(String),
}

/// Dispatch `before_operation` for one call. What the hooks said along the way
/// is added to `notes` for the caller to report its own way.
fn hook_before_call(
    hooks: &arsy_code::hook::HookEngine,
    name: &str,
    arguments: &Value,
    notes: &mut Vec<String>,
) -> HookedCall {
    use arsy_code::hook::{LifecycleEvent, Outcome};

    let before = match hooks.dispatch(LifecycleEvent::BeforeOperation, name, arguments.clone()) {
        Ok(before) => before,
        // The engine's own guards — depth, reentrancy — failing is the harness
        // misbehaving, and `before_operation` fails closed.
        Err(error) => {
            return HookedCall::Refused(format!("the lifecycle engine refused the call: {error}"))
        }
    };
    notes.extend(before.diagnostics);
    match before.outcome {
        Outcome::Deny(reason) => HookedCall::Refused(format!("a hook denied this call: {reason}")),
        // A rewritten payload is what actually runs, which is the whole point
        // of letting a hook transform one.
        outcome => HookedCall::Run {
            arguments: before.payload,
            injected: before.injected,
            approval: match outcome {
                Outcome::RequireApproval(reason) => Some(reason),
                _ => None,
            },
        },
    }
}

/// Dispatch `after_operation` or `operation_failed` for a call that ran, and
/// attach what `before_operation` injected to its result.
///
/// Both events report what already happened, so a failure here is said and the
/// result stands.
fn hook_after_call(
    hooks: &arsy_code::hook::HookEngine,
    name: &str,
    injected: &[(String, String)],
    result: &mut arsy_code::agent::ToolResult,
    notes: &mut Vec<String>,
) {
    use arsy_code::hook::LifecycleEvent;

    // What a hook injected is context the model was meant to see, attributed to
    // the rule that asked for it.
    for (rule, text) in injected {
        result.output.push_str(&format!("\n[hook {rule}] {text}"));
    }
    let after = if result.success {
        LifecycleEvent::AfterOperation
    } else {
        LifecycleEvent::OperationFailed
    };
    let observed = json!({
        "tool": name,
        "success": result.success,
        "output": result.output,
    });
    match hooks.dispatch(after, name, observed) {
        Ok(dispatch) => notes.extend(dispatch.diagnostics),
        Err(error) => notes.push(format!("a hook on `{}` failed: {error}", after.as_str())),
    }
}

/// Run one tool call with the lifecycle events around it, on a surface with no
/// operator to ask.
///
/// `before_operation` sees the call before it happens and may rewrite its
/// arguments, deny it, or ask for an approval nobody is here to give — which,
/// here, is a refusal reported to the model rather than a wait.
/// `after_operation` and `operation_failed` see what it did.
///
/// A refused call is a failed result, not an error: the model asked for
/// something it may not have, and telling it so is how it tries something else.
fn invoke_hooked(
    hooks: Option<&arsy_code::hook::HookEngine>,
    runtime: &arsy_code::agent::ToolRuntime,
    name: &str,
    arguments: &Value,
    emitter: &mut Emitter,
) -> arsy_code::agent::ToolResult {
    let Some(hooks) = hooks else {
        return runtime.invoke(name, arguments);
    };
    let mut notes = Vec::new();
    let result = match hook_before_call(hooks, name, arguments, &mut notes) {
        HookedCall::Refused(reason) => hook_refused(name, reason),
        HookedCall::Run {
            approval: Some(reason),
            ..
        } => hook_refused(
            name,
            format!(
                "a hook asked for the operator's approval, and this surface cannot ask for one: \
                 {reason}"
            ),
        ),
        HookedCall::Run {
            arguments,
            injected,
            approval: None,
        } => {
            let mut result = runtime.invoke(name, &arguments);
            hook_after_call(hooks, name, &injected, &mut result, &mut notes);
            result
        }
    };
    for note in notes {
        emitter.diagnostic(&Diagnostic::warning("ARSY-HOK-1000", note, String::new()));
    }
    result
}

/// The result a call a hook stopped sends back to the model.
fn hook_refused(name: &str, output: String) -> arsy_code::agent::ToolResult {
    arsy_code::agent::ToolResult {
        tool: name.to_owned(),
        success: false,
        output,
        changed_files: Vec::new(),
        duration: std::time::Duration::ZERO,
        metadata: json!({"refused_by": "hook"}),
        artifact: None,
    }
}

/// One counter out of the telemetry summary the run just printed.
fn summary_number(record: &Value, key: &str) -> u64 {
    record["telemetry"][key].as_u64().unwrap_or_default()
}

/// Run one subagent turn to its answer.
///
/// A smaller loop than the parent's on purpose: a child has no operator to ask,
/// no session of its own to record into, and a bound on rounds low enough that
/// a child which cannot answer gives the parent its rounds back rather than
/// spending them. Every tool call still goes through the same runtime — the
/// child's, holding only what was delegated to it.
///
/// `watch` sees a redacted projection of each call: the tool and whether it
/// worked, never the arguments or what came back. Returning
/// [`Intervention::Deny`] stops the child there.
pub(crate) fn child_turn(
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    watch: &mut dyn FnMut(
        &arsy_kernel::observer::RedactedProjection,
    ) -> Option<arsy_kernel::observer::Intervention>,
    emitter: &mut Emitter,
) -> Result<String, String> {
    let mut request = request.clone();
    let base = request.idempotency_key.as_str().to_owned();
    let budget = CONTEXT_BUDGET_TOKENS.saturating_sub(request.max_output_tokens);
    let mut consecutive_failures = 0u64;
    // Tool calls this child has made, so an intervention can be correlated
    // with the call that caused it.
    let mut calls_made = 0u64;
    let mut answer = String::new();

    for round in 0..MAX_CHILD_TOOL_ROUNDS {
        request.idempotency_key =
            IdempotencyKey::new(format!("{base}-{round}")).map_err(|error| error.to_string())?;
        answer.clear();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        let stream =
            arsy_kernel::provider::stream_with_retry(provider, &request, &mut std::thread::sleep)
                .map_err(|error| error.to_string())?;
        for event in stream {
            match event.map_err(|error| error.to_string())? {
                ModelEvent::TextDelta { text } => answer.push_str(&text),
                ModelEvent::ToolCallCompleted {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    emitter.trace(
                        "model.tool_call",
                        json!({"round": round, "id": id, "name": name, "arguments": arguments}),
                    );
                    calls.push((id, name, arguments));
                }
                _ => {}
            }
        }
        if calls.is_empty() {
            return Ok(if answer.trim().is_empty() {
                "the subagent finished without an answer".to_owned()
            } else {
                answer
            });
        }
        arsy_code::agent::budget::fit(&mut request.messages, budget, None);

        let mut content: Vec<ModelContent> = Vec::new();
        if !answer.trim().is_empty() {
            content.push(ModelContent::Text {
                text: answer.clone(),
            });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        request.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });

        let mut results = Vec::with_capacity(calls.len());
        for (id, name, arguments) in &calls {
            let result = runtime.invoke(name, arguments);
            calls_made += 1;
            consecutive_failures = if result.success {
                0
            } else {
                consecutive_failures + 1
            };
            // What an observer is allowed to see: which call this was, the
            // tool, and the outcome. The arguments named a path and the result
            // carried its contents, and neither is the observer's business.
            let projection = arsy_kernel::observer::RedactedProjection {
                sequence: calls_made,
                kind: if result.success {
                    "tool.completed".to_owned()
                } else {
                    "tool.failed".to_owned()
                },
                public_payload: json!({
                    "tool": result.tool,
                    "consecutive_failures": consecutive_failures,
                }),
                redacted_fields: 2,
            };
            let intervened = watch(&projection);
            results.push(ModelContent::ToolResult {
                id: id.clone(),
                content: result.output,
                is_error: !result.success,
            });
            if let Some(arsy_kernel::observer::Intervention::Deny(reason)) = intervened {
                emitter.diagnostic(&Diagnostic::warning(
                    "ARSY-RET-1001",
                    format!("a subagent was stopped: {reason}"),
                    "the parent keeps whatever the subagent had established before it stopped",
                ));
                return Err(format!("stopped by its supervisor: {reason}"));
            }
        }
        request.messages.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
    }
    Err(format!(
        "the subagent used its {MAX_CHILD_TOOL_ROUNDS} rounds without answering"
    ))
}

/// How many rounds of tool calls one subagent may take.
///
/// Fewer than the parent's: a child has one question, and a child that cannot
/// answer it in this many rounds is one the parent should take back.
const MAX_CHILD_TOOL_ROUNDS: usize = 8;

/// What a turn cost, when configuration says what its model charges.
///
/// `None` means nobody wrote a price down. Reported as unknown rather than as
/// zero, because a running total that silently treats every unpriced turn as
/// free is worse than one that admits the gap: the first is wrong and looks
/// right, the second is right about what it does not know.
fn charge_turn(resolved: Option<&provider::Resolved>, model: &str, usage: &Value) -> Option<u64> {
    charge(
        resolved.map(|resolved| &resolved.endpoint),
        model,
        summary_number(usage, "input_tokens"),
        summary_number(usage, "output_tokens"),
    )
}

/// As above, from the parts. A turn that spent no tokens cost nothing at any
/// price, so it is known to be free rather than unknown — otherwise a failed
/// turn would make a whole session's total unknowable.
fn charge(
    endpoint: Option<&arsy_kernel::config::Endpoint>,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<u64> {
    if input_tokens == 0 && output_tokens == 0 {
        return Some(0);
    }
    Some(
        endpoint?
            .pricing
            .get(model)?
            .cost_micros(input_tokens, output_tokens),
    )
}

fn token_usage(input_tokens: u64, output_tokens: u64) -> Value {
    if input_tokens == 0 && output_tokens == 0 {
        json!({})
    } else {
        json!({"input_tokens": input_tokens, "output_tokens": output_tokens})
    }
}

/// Fold `extra`'s fields into `target`, which is always an object here.
fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn prepare_task(
    invocation: &Invocation,
    task: &str,
    emitter: &mut Emitter,
) -> Result<String, Diagnostic> {
    if task.trim().is_empty() {
        return Err(usage("run requires a non-empty task"));
    }
    redactor(invocation, emitter)?
        .sanitize(task)
        .map_err(secret_failed)
}

fn resume(
    invocation: &Invocation,
    session: SessionId,
    follow: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    if store
        .current_version(session)
        .map_err(|error| storage_failed(error.to_string()))?
        .0
        == 0
    {
        return Err(Diagnostic::error(
            "ARSY-SCH-1004",
            format!("session {session} has no recorded events in this workspace"),
            "check the ID, or run `arsy run` with the workspace that recorded it",
        ));
    }

    // Recovery needs no provider: closing what a dead process left open is
    // worth doing even in a workspace that could not dispatch a turn today.
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
        .map_err(storage_failed)?;
    // History is never truncated: a turn that was running when the process
    // died is closed by appending `turn.failed` after the events it already
    // wrote.
    let interrupted = service.unfinished_turns().map_err(storage_failed)?;
    for turn in &interrupted {
        service
            .fail_turn(
                actor.clone(),
                *turn,
                "interrupted",
                "the process exited before the turn finished",
            )
            .map_err(storage_failed)?;
    }
    let version = service.committed_version().map_err(storage_failed)?;
    drop(service);

    // A task whose lease has run out is one nobody is working on, whatever the
    // process that took it intended.
    let mut graph = TaskGraph::new(store, session, actor).map_err(graph_failed)?;
    let mut recovered = graph
        .recover_expired(unix_time_ms())
        .map_err(graph_failed)?;
    // A turn found open is proof its process is gone, so whatever task it held
    // is handed back now rather than when the lease would have run out.
    if !interrupted.is_empty() {
        recovered.extend(graph.reclaim_running().map_err(graph_failed)?);
    }
    let waiting = graph.pending().first().map(|node| node.id);
    drop(graph);

    let mut report = json!({
        "session": session.to_string(),
        "events": version.0,
        "closed_turns": interrupted.len(),
        "recovered_tasks": recovered.len(),
        "continuing": Value::Null,
        // ponytail: following live events needs the serve loop; the flag is
        // accepted and reports the committed head instead of hanging.
        "following": follow,
    });

    let Some(task) = waiting else {
        emitter.result(report);
        return Ok(0);
    };
    // The session has unfinished work. Continuing it needs a provider, so a
    // workspace that cannot dispatch reports the task as still waiting rather
    // than losing it.
    let mut execution = match TaskRun::open(invocation, Some(session)) {
        Ok(execution) => execution,
        Err(diagnostic) => {
            let code = unusable(diagnostic, emitter);
            merge(&mut report, json!({"blocked": task.to_string()}));
            emitter.result(report);
            return Ok(code);
        }
    };
    merge(&mut report, json!({"continuing": task.to_string()}));
    execution.execute(task, report, emitter)
}

fn doctor(invocation: &Invocation, strict: bool, emitter: &mut Emitter) -> i32 {
    let mut warnings: Vec<Diagnostic> = Vec::new();
    let workspace = workspace_root(&invocation.workspace);
    let storage = match &workspace {
        Ok(root) => match open_store(root) {
            Ok(_) => "openable".to_owned(),
            Err(error) => {
                warnings.push(error.clone());
                error.message
            }
        },
        Err(error) => {
            warnings.push(error.clone());
            error.message.clone()
        }
    };

    // ponytail: layer discovery only. Merged values and their source trace
    // arrive with `arsy config explain`.
    //
    // Bootstrapped first, so the user layer is reported as it will be for
    // every later command rather than as absent on the run that creates it.
    bootstrap_user_config();
    let root = workspace.as_deref().unwrap_or(Path::new("."));
    let config: Vec<Value> = arsy_kernel::config::layers(root, root)
        .into_iter()
        .map(|(layer, path)| {
            json!({
                "layer": layer,
                "path": path.display().to_string(),
                "present": path.is_file(),
            })
        })
        .collect();

    // A configured endpoint with a reachable credential is what decides
    // whether a turn can dispatch, so report it as one fact rather than
    // leaving an operator to infer it from the credential count.
    let provider = match load_config(root, root, invocation.config.as_deref()) {
        Err(diagnostic) => {
            let value = json!({"status": "unusable", "detail": diagnostic.message});
            warnings.push(diagnostic);
            value
        }
        Ok(config) => match provider::resolve(&config, None) {
            Ok(resolved) => json!({
                "status": "ready",
                "id": resolved.endpoint.id,
                "kind": resolved.endpoint.kind.as_str(),
                "base_url": resolved.endpoint.base_url,
                "credential_source": resolved.source.as_str(),
                // Present only when `provider.default = "auto"` left the choice
                // to routing; naming the criterion is what makes the choice
                // reviewable rather than surprising.
                "routing": resolved.route.as_ref().map(|decision| match decision {
                    arsy_kernel::routing::Decision::Routed { key, reasons, excluded } => json!({
                        "model": key.to_string(),
                        "reasons": reasons,
                        "excluded": excluded.len(),
                    }),
                    other => serde_json::to_value(other).unwrap_or(Value::Null),
                }),
            }),
            Err(diagnostic) => json!({"status": "unavailable", "detail": diagnostic.message}),
        },
    };

    let sandbox_assurance = installed_sandbox_assurance();
    if sandbox_assurance == arsy_kernel::policy::SandboxAssurance::None {
        warnings.push(Diagnostic::warning(
            "ARSY-SBX-1000",
            "no complete sandbox worker is available, so achieved assurance is `none`",
            "install arsy-sandbox-worker and the platform controls before running effects",
        ));
    }
    let credentials = catalog(CatalogStore::resolve(invocation))
        .unwrap_or_default()
        .len();
    if credentials == 0 {
        warnings.push(Diagnostic::warning(
            "ARSY-PRV-1001",
            "no provider credential is stored",
            "store a credential with `arsy auth set <PROVIDER>`",
        ));
    }

    for warning in &warnings {
        emitter.diagnostic(warning);
    }
    emitter.result(json!({
        "platform": platform(),
        "version": arsy_code::VERSION,
        "sandbox_assurance": sandbox_assurance.as_str(),
        "provider_auth": if credentials == 0 { "none" } else { "configured" },
        "provider": provider,
        "storage": storage,
        "config_layers": config,
        "warnings": warnings.len(),
    }));

    // Warnings return 0 unless --strict; then the first reported one is
    // terminal and selects the code.
    match (strict, warnings.first()) {
        (true, Some(terminal)) => terminal.exit_code(),
        _ => 0,
    }
}

fn installed_sandbox_assurance() -> arsy_kernel::policy::SandboxAssurance {
    let Ok(executable) = std::env::current_exe() else {
        return arsy_kernel::policy::SandboxAssurance::None;
    };
    let worker = executable.with_file_name(if cfg!(windows) {
        "arsy-sandbox-worker.exe"
    } else {
        "arsy-sandbox-worker"
    });
    if !worker.is_file() {
        return arsy_kernel::policy::SandboxAssurance::None;
    }
    arsy_code::sandbox::PlatformSandbox::detect()
        .map_or(arsy_kernel::policy::SandboxAssurance::None, |backend| {
            backend.assurance()
        })
}

fn read_stdin() -> Result<String, Diagnostic> {
    let mut task = String::new();
    io::stdin()
        .read_to_string(&mut task)
        .map_err(|error| usage(format!("stdin is not readable UTF-8 text: {error}")))?;
    Ok(task)
}

fn workspace_root(requested: &Path) -> Result<PathBuf, Diagnostic> {
    std::fs::canonicalize(requested).map_err(|error| {
        usage(format!(
            "workspace {} is unusable: {error}",
            requested.display()
        ))
    })
}

fn open_store(workspace: &Path) -> Result<Arc<SqliteEventStore>, Diagnostic> {
    let path = workspace.join(STORE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| storage_failed(error.to_string()))?;
    }
    SqliteEventStore::open(&path, Durability::Normal)
        .map(Arc::new)
        .map_err(|error| storage_failed(format!("{path:?}: {error}")))
}

fn storage_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-CMP-1000",
        format!("the session store failed: {}", error.to_string()),
        "check the workspace `.arsy` directory is writable and not held by another process",
    )
}

fn actor() -> Principal {
    Principal::User(
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "local".to_owned()),
    )
}

/// Build the tool runtime a turn executes through.
///
/// The same construction `arsy serve` performs, because a turn and a served
/// call have to reach the same operations under the same rules; the only
/// difference is the risk context, which says whether an operator is present to
/// answer an approval.
/// The workspace's artifact store: where every operation's result, every
/// exported excerpt, and every memory's claim is kept.
fn artifact_store(root: &Path) -> Result<arsy_kernel::artifact::FileArtifactStore, Diagnostic> {
    arsy_kernel::artifact::FileArtifactStore::open(root.join(".arsy/artifacts"), 0)
        .map_err(|error| storage_failed(error.to_string()))
}

/// `scope` isolates the plan and validation history this runtime's
/// `plan.*`/`validate.*` kinds hold from another unit of work in the same
/// workspace — a session id for a turn, a task id for an autonomous task, or
/// any other value unique to the caller. See `operations::registry`.
/// `session` is the stream durable state belongs to. Without one — a dry run,
/// or a build with no store open — the runtime offers no `todo.*` tool rather
/// than a checklist that would vanish when the process ends.
#[allow(clippy::too_many_arguments)]
fn agent_runtime(
    root: &Path,
    config: &arsy_kernel::config::Config,
    interactive: bool,
    scope: &str,
    session: Option<SessionId>,
    connector: Option<&connector::McpConnector>,
    emitter: &mut Emitter,
) -> Result<arsy_code::agent::ToolRuntime, Diagnostic> {
    let workspace = arsy_code::resource::Workspace::open(root)
        .map_err(|error| storage_failed(error.to_string()))?;
    let artifacts = Arc::new(artifact_store(root)?);
    // Connections are opened here, once, because this is the turn boundary:
    // the set of tools the model is told about and the set a call can reach
    // are then the same set by construction.
    //
    // Only for a turn that has a session. A dry run or a one-shot inspection
    // has no conversation to offer tools to, and starting somebody's MCP
    // server as a side effect of `arsy code symbol` would be a surprise.
    //
    // An interactive session holds its connections across turns instead, and
    // only brings them in line with configuration here.
    let (connections, pending, discovered) = match (connector, session) {
        (Some(connector), _) => session_mcp(connector, config, emitter),
        (None, Some(_)) => {
            let (connections, discovered) = mcp::connect_enabled(config, emitter);
            (connections, Default::default(), discovered)
        }
        (None, None) => (None, Default::default(), Vec::new()),
    };
    arsy_code::agent::runtime(
        &workspace,
        config.policy_rule_set(),
        artifacts,
        arsy_kernel::artifact::unix_time_ms(),
        actor(),
        arsy_kernel::policy::RiskContext {
            reversible: interactive,
            workspace: arsy_code::git::cleanliness(root)
                .unwrap_or(arsy_kernel::policy::WorkspaceCleanliness::Unknown),
            sandbox: installed_sandbox_assurance(),
        },
        arsy_code::operations::Reachable::from_config(config),
        scope,
        arsy_code::operations::TurnState {
            journal: session
                .map(|session| -> Result<_, Diagnostic> {
                    Ok(arsy_code::agent::todoops::Journal {
                        store: open_store(root)? as Arc<dyn EventStore>,
                        session,
                        actor: actor(),
                    })
                })
                .transpose()?,
            mcp: connections,
            mcp_pending: pending,
        },
    )
    .map(|runtime| runtime.with_dynamic_tools(discovered))
    .map_err(|error| storage_failed(error.to_string()))
}

/// The session's MCP connections for this turn, and what failed since the last.
fn session_mcp(
    connector: &connector::McpConnector,
    config: &arsy_kernel::config::Config,
    emitter: &mut Emitter,
) -> (
    Option<arsy_code::agent::mcpops::Connections>,
    arsy_code::agent::mcpops::Pending,
    Vec<arsy_code::agent::DynamicTool>,
) {
    let discovered = connector.sync(config);
    for failure in connector.failures() {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-MCP-1000",
            failure,
            "check it with `arsy mcp test <NAME>`, or switch it off in /mcp",
        ));
    }
    let (connections, pending) = connector.session();
    (Some(connections), pending, discovered)
}

/// The MCP connections of this interactive session, held until it ends.
///
/// One per process, because an interactive session is one process: every turn
/// and every resumed session in it reuses the same servers.
#[cfg(feature = "tui")]
fn session_connector() -> &'static connector::McpConnector {
    static CONNECTOR: std::sync::OnceLock<connector::McpConnector> = std::sync::OnceLock::new();
    CONNECTOR.get_or_init(|| {
        connector::McpConnector::new(
            arsy_kernel::config::config_home().map(|home| home.join("mcp-tools.json")),
        )
    })
}

/// The system prompt for one turn: the harness's own instructions, then the
/// project's, discovered by walking from the workspace root to the working
/// directory.
///
/// Compilation failure is not a reason to lose the turn — a prompt over budget
/// or an unredactable secret is a degradation, not a fault — so the harness
/// instructions alone are the floor.
/// What a recalled memory may take out of the prompt.
///
/// Small on purpose. Memory competes with the task and the repository's own
/// instructions for the same window, and a workspace that remembers a page of
/// facts is one whose next turn has less room to read the code.
const MAX_RECALLED_MEMORY_BYTES: usize = 4 * 1024;

/// The plugins this workspace has installed and approved, for the prompt.
///
/// Read fresh each turn rather than cached: `arsy plugin install` and
/// `arsy plugin refresh` take effect at a turn boundary, and a listing held
/// from session start would tell the model about a set that no longer exists.
///
/// Only loadable plugins are listed. One whose manifest now asks for more than
/// was approved cannot run, and offering it would produce a refusal the model
/// could do nothing about.
#[cfg(feature = "wasm")]
fn installed_extensions(root: &Path) -> Vec<arsy_code::agent::instructions::ExtensionTool> {
    let (installed, _unreadable) = arsy_code::plugin::Registry::open(root)
        .list()
        .unwrap_or_default();
    installed
        .into_iter()
        .filter(arsy_code::plugin::Installed::loadable)
        .map(|plugin| arsy_code::agent::instructions::ExtensionTool {
            id: plugin.manifest.id,
            version: plugin.manifest.version,
            capabilities: plugin
                .manifest
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
        .collect()
}

/// A build without the WASM host can install nothing, so it lists nothing.
#[cfg(not(feature = "wasm"))]
fn installed_extensions(_root: &Path) -> Vec<arsy_code::agent::instructions::ExtensionTool> {
    Vec::new()
}

fn system_prompt(
    root: &Path,
    config: &arsy_kernel::config::Config,
    provider: &str,
    model: &str,
    mode: arsy_code::agent::ExecutionMode,
) -> Option<String> {
    let workspace = arsy_code::resource::Workspace::open(root).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
    let instructions = instructions_for(&workspace, config, &working);
    let family = arsy_code::agent::instructions::family_for(provider, model);
    let compiled = arsy_code::agent::instructions::system_prompt(
        family,
        &instructions,
        &installed_extensions(root),
        memory::recalled(root, MAX_RECALLED_MEMORY_BYTES).as_deref(),
        mode,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .ok()?;
    Some(arsy_code::agent::instructions::render(&compiled))
}

/// The operator's own Claude Code and Codex instructions, then the
/// repository's, root first.
///
/// The switches come from the invocation's resolved config, `--config`
/// included, so the prompt follows the same compat policy as everything else.
fn instructions_for(
    workspace: &arsy_code::resource::Workspace,
    config: &arsy_kernel::config::Config,
    working: &Path,
) -> Vec<arsy_code::agent::instructions::Instruction> {
    use arsy_code::agent::instructions::{self, Instruction, MAX_INSTRUCTION_BYTES};
    let enabled = |source: &str| config.compat_enabled(source);
    arsy_compat::instructions::user_instructions(
        &compat_homes(),
        enabled("claude"),
        enabled("codex"),
        MAX_INSTRUCTION_BYTES,
    )
    .into_iter()
    .map(|found| Instruction {
        path: found.path.display().to_string(),
        text: found.text,
        truncated: found.truncated,
        operator: true,
    })
    .chain(instructions::discover_with(
        workspace,
        working,
        enabled("claude"),
    ))
    .collect()
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        event::{EventPayload, EventStore},
        protocol::{ClientRequest, ProtocolEnvelope, TurnStart},
    };

    /// The environment is the process's, so tests that set a variable take
    /// this lock rather than reading each other's.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A first run creates the settings file and never replaces one that is
    /// already there.
    #[test]
    #[cfg(unix)]
    fn replacing_a_settings_file_is_atomic_and_keeps_the_mode_it_had() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("nested").join("arsy.json");

        // A file that is not there yet is created, directories and all.
        replace_file(&path, b"{}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");

        // An operator who tightened the file keeps that across a write: the
        // staged file is new, so it would otherwise carry only the umask.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        replace_file(&path, b"{ }\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ }\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a tightened settings file is not loosened by the next write"
        );

        // Nothing staged is left behind for the next reader to trip over.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "arsy.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_first_run_creates_the_settings_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|held| held.into_inner());
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join(".arsy");
        let previous = std::env::var_os(arsy_kernel::config::CONFIG_HOME_VAR);
        std::env::set_var(arsy_kernel::config::CONFIG_HOME_VAR, &home);

        bootstrap_user_config();
        let path = home.join(arsy_kernel::config::CONFIG_FILE);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        // An empty settings file is a usable one.
        assert!(arsy_kernel::config::Config::load(&[(
            arsy_kernel::config::Layer::User,
            path.clone()
        )])
        .is_ok());

        // What is there already is never replaced.
        std::fs::write(&path, "{\"model\": {\"default\": \"m1\"}}\n").unwrap();
        bootstrap_user_config();
        assert!(std::fs::read_to_string(&path).unwrap().contains("m1"));

        match previous {
            Some(value) => std::env::set_var(arsy_kernel::config::CONFIG_HOME_VAR, value),
            None => std::env::remove_var(arsy_kernel::config::CONFIG_HOME_VAR),
        }
    }

    /// An operator who already had the older TOML keeps every setting it held,
    /// converted once into the file the new layer reads.
    #[test]
    fn the_settings_an_older_arsy_kept_are_carried_over() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = directory
            .path()
            .join(arsy_kernel::config::LEGACY_CONFIG_FILE);
        std::fs::write(
            &legacy,
            "schema_version = 1\n[model]\ndefault = \"m1\"\nallowed = [\"m1\"]\n",
        )
        .unwrap();

        let raw = std::fs::read_to_string(&legacy).unwrap();
        let carried = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        std::fs::write(
            &carried,
            arsy_kernel::config::json_from_toml(&raw, &carried).unwrap(),
        )
        .unwrap();

        let config =
            arsy_kernel::config::Config::load(&[(arsy_kernel::config::Layer::User, carried)])
                .unwrap();
        assert_eq!(config.model_default(), Some("m1"));
        assert!(
            !config.model_is_allowed("m9"),
            "the ceiling came across too"
        );
    }

    /// Write a configuration file, given as TOML and converted.
    ///
    /// The schema reads more clearly as TOML than as quoted JSON; what reaches
    /// disk is the `arsy.json` a real run loads.
    fn write_config_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let json = arsy_kernel::config::json_from_toml(body, path).unwrap();
        std::fs::write(path, json).unwrap();
    }

    /// Two things a stored credential must not do to a turn that never asks
    /// for it: abort the turn because it will not open, and follow a run that
    /// was pointed at a throwaway configuration home into that home.
    #[test]
    fn a_credential_that_will_not_open_neither_fails_the_turn_nor_follows_a_throwaway_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|held| held.into_inner());
        let home = std::env::temp_dir().join(format!("arsy-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(arsy_kernel::config::CONFIG_HOME_VAR, &home);
        let path = home.join(CATALOG_FILE);
        let record = AuthRecord {
            provider: "unreachable".to_owned(),
            handle: SecretHandle::new(OS_STORE_ID, "arsy-no-such-credential").unwrap(),
            created_at: 0,
            last_used: None,
            kind: CredentialKind::default(),
        };
        let raw = serde_json::to_string(&[record]).unwrap();
        owner_only(&path)
            .unwrap()
            .write_all(raw.as_bytes())
            .unwrap();

        let invocation = Invocation {
            debug: false,
            config: None,
            provider: None,
            model: None,
            workspace: PathBuf::from("."),
            output: None,
            no_color: true,
            command: Command::Tui,
        };
        redactor(&invocation, &mut Emitter::new(Output::Ci))
            .expect("a handle that will not open leaves the turn alone");

        std::fs::remove_file(&path).unwrap();
        assert!(
            catalog(CatalogStore::File).unwrap().is_empty(),
            "an explicit config home is not backfilled from the operator's platform store"
        );
        assert!(
            !path.exists(),
            "nothing was migrated into the throwaway home"
        );

        std::env::remove_var(arsy_kernel::config::CONFIG_HOME_VAR);
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// A provider that replays a scripted round per request and records what
    /// it was asked, so a test can assert on the conversation the loop built.
    #[cfg(feature = "tui")]
    struct Scripted {
        descriptor: arsy_kernel::provider::ProviderDescriptor,
        rounds: std::sync::Mutex<std::collections::VecDeque<Vec<ModelEvent>>>,
        seen: std::sync::Mutex<Vec<CanonicalModelRequest>>,
    }

    #[cfg(feature = "tui")]
    impl arsy_kernel::provider::ModelProvider for Scripted {
        fn descriptor(&self) -> &arsy_kernel::provider::ProviderDescriptor {
            &self.descriptor
        }

        fn stream(
            &self,
            request: &CanonicalModelRequest,
        ) -> Result<arsy_kernel::provider::ModelEventStream, arsy_kernel::provider::ProviderError>
        {
            self.seen.lock().unwrap().push(request.clone());
            let events = self.rounds.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::new(events.into_iter().map(Ok)))
        }
    }

    #[cfg(feature = "tui")]
    fn resolved(rounds: Vec<Vec<ModelEvent>>) -> (provider::Resolved, std::sync::Arc<Scripted>) {
        let scripted = std::sync::Arc::new(Scripted {
            descriptor: arsy_kernel::provider::ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 0,
            },
            rounds: std::sync::Mutex::new(rounds.into()),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let resolved = provider::Resolved {
            provider: scripted.clone(),
            endpoint: arsy_kernel::config::Endpoint {
                pricing: std::collections::BTreeMap::new(),
                id: "stub".to_owned(),
                kind: arsy_kernel::config::Dialect::Openai,
                base_url: "https://stub.invalid/v1".to_owned(),
                credential: None,
                api_key_env: None,
                model: Some("m".to_owned()),
                models: vec!["m".to_owned()],
                max_output_tokens: 64,
                oauth: None,
            },
            source: provider::CredentialSource::DefaultEnv,
            route: None,
        };
        (resolved, scripted)
    }

    #[cfg(feature = "tui")]
    fn route() -> tui::ModelRoute {
        tui::ModelRoute {
            provider: "stub".to_owned(),
            model: "m".to_owned(),
        }
    }

    /// A runtime over a scratch workspace, under whatever policy an unconfigured
    /// workspace gets — which is what a first run actually sees.
    #[cfg(feature = "tui")]
    fn test_runtime(root: &Path) -> arsy_code::agent::ToolRuntime {
        agent_runtime(
            root,
            &load_config(root, root, None).unwrap(),
            true,
            "test",
            None,
            None,
            &mut Emitter::new(Output::Json),
        )
        .unwrap()
    }

    /// Answers typed at the confirmation prompt. Keys sent while a round is
    /// still streaming belong to the composer, exactly as they do in a
    /// session, so an answer only counts once the prompt is up.
    ///
    /// There is no way to observe the prompt appearing from here, and sleeping
    /// long enough to assume it has is a race that a loaded runner loses: the
    /// composer eats the answer mid-stream, the prompt then blocks until the
    /// sender hangs up, and the turn stops with an empty response. So the
    /// answer is offered until the caller reports the turn finished, the way a
    /// person waiting on a prompt presses again.
    ///
    /// The returned flag must be set once the turn returns, or the join hangs.
    #[cfg(feature = "tui")]
    fn typed(
        answers: &'static [u8],
        approval: std::sync::Arc<approval::ApprovalCell>,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<u8>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (sender, keys) = std::sync::mpsc::channel();
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let finished = std::sync::Arc::clone(&done);
        let typist = std::thread::spawn(move || {
            for (answered, answer) in answers.iter().enumerate() {
                // Wait for the prompt this answer belongs to. Sending before it
                // is up hands the byte to the composer instead, which is what a
                // session does with a keystroke typed mid-stream.
                while approval.opened() <= answered {
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                if sender.send(*answer).is_err() {
                    return;
                }
            }
            // The sender stays alive until the turn ends: a confirmation that
            // never comes must block, not read as a hung-up keyboard.
            while !finished.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            drop(sender);
        });
        (typist, keys, done)
    }

    /// A hook that denies every call.
    struct DenyAll;

    impl arsy_code::hook::HookHandler for DenyAll {
        fn run(
            &self,
            _rule: &arsy_code::hook::HookRule,
            _payload: &Value,
        ) -> Result<arsy_code::hook::HandlerResult, arsy_code::hook::HookError> {
            Ok(arsy_code::hook::HandlerResult {
                outcome: Some(arsy_code::hook::Outcome::Deny("no patches here".to_owned())),
                payload: None,
                inject: None,
                schedule: None,
            })
        }
    }

    /// The interactive turn dispatches `before_operation`, so a Claude hook
    /// that blocks a call blocks it here too, before anyone is asked.
    #[cfg(feature = "tui")]
    #[test]
    fn a_hook_that_denies_a_call_stops_it_in_the_interactive_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let patch = "*** Begin Patch\n*** Add File: note.txt\n+blocked\n*** End Patch\n";
        let (resolved, _) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments: json!({ "patch": patch }),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "understood\n".to_owned(),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        let mut hooks = arsy_code::hook::HookEngine::new(1);
        hooks.register(
            arsy_code::hook::HookRule {
                id: "deny".to_owned(),
                event: arsy_code::hook::LifecycleEvent::BeforeOperation,
                matcher: "*".to_owned(),
                effect: arsy_code::hook::EffectClass::Gate,
                origin: arsy_kernel::capability::PolicySource::User,
                timeout: std::time::Duration::from_secs(5),
            },
            Box::new(DenyAll),
        );
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"", std::sync::Arc::clone(&approval));
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "write a note".to_owned(),
            }],
        }];
        let turn = native_turn(
            &resolved,
            &arsy_kernel::config::Config::default(),
            &test_runtime(workspace.path()),
            &mut conversation,
            &arsy_code::agent::budget::History::default(),
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &mut tui::Transcript::default(),
            &approval,
            Some(&hooks),
        )
        .unwrap();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        typist.join().unwrap();

        assert_eq!(turn.response.trim(), "understood");
        assert!(
            !workspace.path().join("note.txt").exists(),
            "the call never ran"
        );
        assert_eq!(approval.opened(), 0, "nobody was asked about a denied call");
        assert!(matches!(
            conversation[2].content.first(),
            Some(ModelContent::ToolResult { is_error: true, content, .. })
                if content.contains("no patches here")
        ));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_confirmed_tool_call_runs_and_its_result_goes_back_to_the_model() {
        let workspace = tempfile::tempdir().unwrap();
        let patch =
            "*** Begin Patch\n*** Add File: note.txt\n+written by the tool loop\n*** End Patch\n";
        let (resolved, scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments: json!({ "patch": patch }),
                },
                ModelEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "done\n".to_owned(),
                },
                ModelEvent::Usage {
                    input_tokens: 300,
                    output_tokens: 5,
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"y", std::sync::Arc::clone(&approval));
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "write a note".to_owned(),
            }],
        }];
        let turn = native_turn(
            &resolved,
            &arsy_kernel::config::Config::default(),
            &test_runtime(workspace.path()),
            &mut conversation,
            &arsy_code::agent::budget::History::default(),
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &mut tui::Transcript::default(),
            &approval,
            None,
        )
        .unwrap();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        typist.join().unwrap();

        assert_eq!(turn.response.trim(), "done");
        // Both requests are charged to the turn, not just the last one.
        assert_eq!(
            turn.usage,
            json!({"input_tokens": 400, "output_tokens": 15})
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "written by the tool loop\n"
        );
        // The call and its result are both history, in that order.
        assert!(matches!(
            conversation[1].content.first(),
            Some(ModelContent::ToolCall { name, .. }) if name == "apply_patch"
        ));
        assert!(matches!(
            conversation[2].content.first(),
            Some(ModelContent::ToolResult { is_error: false, content, .. })
                if content.contains("added note.txt")
        ));
        // Both requests offered the tools, and the second carried the result.
        let seen = scripted.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the loop asked again after the tool ran");
        // The semantic tools are offered between search and editing, and a
        // build with the WASM feature offers `plugin.invoke` as well; what
        // this asserts is the order of the rest.
        let offered: Vec<&str> = seen[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .filter(|name| !name.starts_with("code.") && *name != "plugin.invoke")
            .collect();
        assert_eq!(
            offered,
            [
                "fs.read",
                "fs.list",
                "search.files",
                "search.text",
                "fs.edit",
                "apply_patch",
                "fs.write",
                "fs.delete",
                "fs.move",
                "bash",
                "web_fetch",
                "bash_start",
                "bash_poll",
                "bash_write",
                "bash_stop",
                "repo_map",
                "repo_discover",
                "git_status",
                "git_branch",
                "git_diff",
                "git_log",
                "git_blame",
                "plan_add",
                "plan_update",
                "plan_remove",
                "plan_reorder",
                "plan_list",
                "validate_record",
                "validate_status",
            ],
            "reading and searching are offered before the shell"
        );
        // The system prompt is built, not omitted: a turn that tells the model
        // nothing about the workspace is the bug this replaced.
        let system = seen[0].system.as_deref().unwrap_or_default();
        assert!(system.contains("ARSY"), "{system}");
        assert_eq!(seen[1].messages.len(), 3);
        assert_ne!(
            seen[0].idempotency_key, seen[1].idempotency_key,
            "each round is its own request"
        );
    }
    #[cfg(feature = "tui")]
    #[test]
    fn a_successful_duplicate_command_runs_once_and_finishes_the_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let command = "printf x >> duplicate-command-marker";
        let (resolved, scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": command}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-2".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": command, "timeout_ms": 600000}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
        ]);
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Auto));
        let (_keys_sender, keys) = std::sync::mpsc::channel();
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "run the command once".to_owned(),
            }],
        }];
        let turn = native_turn(
            &resolved,
            &arsy_kernel::config::Config::default(),
            &test_runtime(workspace.path()),
            &mut conversation,
            &arsy_code::agent::budget::History::default(),
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &mut tui::Transcript::default(),
            &approval,
            None,
        )
        .unwrap();

        assert!(turn.failure.is_none(), "{:?}", turn.failure);
        assert!(turn.response.contains("repeated tool call was skipped"));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("duplicate-command-marker")).unwrap(),
            "x"
        );
        let seen_count = scripted
            .seen
            .lock()
            .map(|seen| seen.len())
            .unwrap_or_default();
        assert_eq!(
            seen_count, 2,
            "the duplicate was stopped before another provider round"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_declined_tool_call_does_not_run_and_the_model_is_told_so() {
        let workspace = tempfile::tempdir().unwrap();
        let (resolved, _scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch escaped"}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "understood\n".to_owned(),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        // `d` denies; `n` now opens the note editor.
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"d", std::sync::Arc::clone(&approval));
        let mut conversation = Vec::new();
        let turn = native_turn(
            &resolved,
            &arsy_kernel::config::Config::default(),
            &test_runtime(workspace.path()),
            &mut conversation,
            &arsy_code::agent::budget::History::default(),
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &mut tui::Transcript::default(),
            &approval,
            None,
        )
        .unwrap();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        typist.join().unwrap();

        assert_eq!(turn.response.trim(), "understood");
        assert!(
            !turn.interrupted,
            "declining one call is not a stopped turn"
        );
        assert!(
            !workspace.path().join("escaped").exists(),
            "a declined command must not run"
        );
        assert!(matches!(
            conversation[1].content.first(),
            Some(ModelContent::ToolResult { is_error: true, content, .. })
                if content.contains("declined")
        ));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn ctrl_c_at_the_prompt_stops_the_turn_instead_of_declining_one_call() {
        let workspace = tempfile::tempdir().unwrap();
        // Two calls in one round, and a second round that would follow. Neither
        // may run, and the loop must not ask the provider again.
        let asking = || {
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch first"}),
                },
                ModelEvent::ToolCallCompleted {
                    index: 1,
                    id: "call-2".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch second"}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ]
        };
        let (resolved, scripted) = resolved(vec![asking(), asking()]);
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"\x03", std::sync::Arc::clone(&approval));
        let mut conversation = Vec::new();
        // A stop is answered by the stop, not by waiting for the keyboard to
        // hang up: the calls after it are refused without asking.
        let turn = native_turn(
            &resolved,
            &arsy_kernel::config::Config::default(),
            &test_runtime(workspace.path()),
            &mut conversation,
            &arsy_code::agent::budget::History::default(),
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &mut tui::Transcript::default(),
            &approval,
            None,
        )
        .unwrap();
        // The typist outlives the turn on purpose, so an unanswered prompt
        // blocks rather than reading as a hung-up keyboard.
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        typist.join().unwrap();

        assert!(turn.interrupted, "Ctrl-C at the prompt ends the turn");
        // No wall-clock budget here. That the stop carried to the calls after
        // it is what the assertions below prove; a clock on a shared runner
        // measures the runner. A stop that failed to carry would leave the
        // second prompt waiting on a keyboard that never answers, so it would
        // hang rather than run slow.
        assert!(!workspace.path().join("first").exists());
        assert!(
            !workspace.path().join("second").exists(),
            "the calls after the stop must not run either"
        );
        assert_eq!(
            scripted.seen.lock().unwrap().len(),
            1,
            "a stopped turn does not ask the provider again"
        );
        // Every call still has a result, because a provider that sent one and
        // never sees an answer rejects the next request.
        let results = &conversation[1].content;
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|content| matches!(content, ModelContent::ToolResult { is_error: true, .. })));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn slash_commands_expand_to_the_inspection_the_cli_already_parses() {
        let expansion = |line: &str| inspection_args(line).map(|args| args.join(" "));

        // A bare command takes its default subcommand; a flag is not one, so it
        // does not suppress the default the way a subcommand does.
        assert_eq!(expansion("/mcp"), Some("mcp list".to_owned()));
        assert_eq!(
            expansion("/mcp --source claude"),
            Some("mcp list --source claude".to_owned())
        );
        assert_eq!(
            expansion("/mcp show NAME"),
            Some("mcp show NAME".to_owned()),
            "an explicit subcommand is not replaced"
        );
        assert_eq!(expansion("/hooks"), Some("hook list".to_owned()));
        assert_eq!(expansion("/settings"), Some("config explain".to_owned()));
        assert_eq!(
            expansion("/settings model.route"),
            Some("config explain model.route".to_owned())
        );
        assert_eq!(expansion("/doctor"), Some("doctor".to_owned()));
        assert_eq!(expansion("/auth"), Some("auth list".to_owned()));
        assert_eq!(
            expansion("/compat claude"),
            Some("compat explain claude".to_owned())
        );
        assert_eq!(expansion("/nonsense"), None, "the loop reports it instead");

        // Every expansion is a command the CLI parser already accepts, so the
        // TUI adds no second argument grammar to keep in step.
        for line in [
            "/mcp",
            "/hooks --event PreToolUse",
            "/settings",
            "/doctor",
            "/auth",
            "/compat omp",
        ] {
            let args = inspection_args(line).expect("mapped");
            assert!(parse(args).is_ok(), "{line} did not parse");
        }

        // Auth commands expand to their CLI equivalents.
        for line in ["/auth remove secret://os/handle", "/auth login codex"] {
            let args = inspection_args(line).expect("mapped");
            assert!(parse(args).is_ok(), "{line} did not parse");
        }
    }

    /// The catalog names every provider the operator holds a credential for, so
    /// it is created owner-only rather than narrowed after the fact.
    #[test]
    fn a_catalog_file_is_never_briefly_world_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");

        {
            let mut file = owner_only(&path).unwrap();
            file.write_all(b"[]\n").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "created as {mode:04o}");

            // Rewriting truncates rather than appending, and does not widen the
            // mode a second time.
            let mut file = owner_only(&path).unwrap();
            file.write_all(b"[]").unwrap();
            drop(file);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "rewritten as {mode:04o}");
        }
    }

    /// The model picker answers a configured endpoint's list the same way it
    /// answers Codex's: by number, by slug, or not at all.
    #[cfg(feature = "tui")]
    #[test]
    fn the_model_picker_answers_a_configured_endpoints_list() {
        let route = tui::ModelRoute {
            provider: "hari".to_owned(),
            model: "mimo".to_owned(),
        };
        let listed: Vec<tui::ModelChoice> = ["mimo", "mimo-2", "mimo-lite"]
            .into_iter()
            .map(|slug| tui::ModelChoice {
                provider: "hari".to_owned(),
                slug: slug.to_owned(),
                name: "on hari".to_owned(),
            })
            .collect();

        // A number picks from the list; the row names the provider it serves,
        // so the route follows the row rather than the current provider.
        let picked = tui::resolve_model("2", &listed, &route).unwrap();
        assert_eq!(picked.model, "mimo-2");
        assert_eq!(picked.provider, "hari");

        // A slug that is not on the list is still accepted, because the list is
        // what the endpoint advertises, not what it will refuse.
        assert_eq!(
            tui::resolve_model("mimo-preview", &listed, &route)
                .unwrap()
                .model,
            "mimo-preview"
        );

        // Out of range says the range rather than silently keeping the current.
        let error = tui::resolve_model("9", &listed, &route).unwrap_err();
        assert!(error.contains("1-3"), "{error}");

        // An empty answer keeps what is set.
        assert_eq!(tui::resolve_model("  ", &listed, &route).unwrap(), route);

        // An endpoint that lists nothing says so instead of naming a range.
        let error = tui::resolve_model("1", &[], &route).unwrap_err();
        assert!(error.contains("no models are listed"), "{error}");
    }

    /// Every question `/provider` asks validates its own answer, and nothing
    /// reaches the configuration until the last one.
    #[cfg(feature = "tui")]
    #[test]
    fn the_provider_wizard_validates_each_answer_before_it_moves_on() {
        use tui::ProviderStep as Step;

        let invocation = Invocation {
            debug: false,
            config: None,
            provider: None,
            model: None,
            workspace: PathBuf::from("."),
            output: None,
            no_color: true,
            command: Command::Tui,
        };
        let providers = vec!["myai".to_owned()];
        let mut draft = tui::ProviderDraft::default();
        let step = |step: Step, line: &str, draft: &mut tui::ProviderDraft| {
            provider_step(&invocation, step, line, draft, &providers)
        };

        // An empty answer leaves the wizard rather than writing a blank field.
        assert!(matches!(
            step(Step::Name, "   ", &mut draft),
            Ok(ProviderNext::Cancelled(_))
        ));

        // The two actions are rows, not provider names.
        assert!(matches!(
            step(Step::Pick, "+new", &mut draft),
            Ok(ProviderNext::Ask(Step::Name))
        ));
        assert!(matches!(
            step(Step::Pick, "-remove", &mut draft),
            Ok(ProviderNext::Ask(Step::Remove))
        ));
        assert!(step(Step::Pick, "nothere", &mut draft).is_err());

        // A name has to be new, writable, and not look like an action row.
        assert!(
            step(Step::Name, "myai", &mut draft).is_err(),
            "duplicate name"
        );
        assert!(step(Step::Name, "+new", &mut draft).is_err(), "action name");
        assert!(step(Step::Name, "has \"quote\"", &mut draft).is_err());
        assert!(matches!(
            step(Step::Name, "acme", &mut draft),
            Ok(ProviderNext::Ask(Step::Kind))
        ));
        assert_eq!(draft.name, "acme");

        // The dialect and the store are closed sets.
        assert!(step(Step::Kind, "gemini", &mut draft).is_err());
        assert!(matches!(
            step(Step::Kind, "anthropic", &mut draft),
            Ok(ProviderNext::Ask(Step::BaseUrl))
        ));

        // A base URL has to be one.
        assert!(step(Step::BaseUrl, "acme.test", &mut draft).is_err());
        assert!(matches!(
            step(Step::BaseUrl, "https://acme.test/v1", &mut draft),
            Ok(ProviderNext::Ask(Step::Model))
        ));
        // One host serves several models, so the step takes a list; a slug that
        // could not be written into TOML is refused before any of it is kept.
        assert!(step(Step::Model, "acme-1, bad\"quote", &mut draft).is_err());
        assert!(
            step(Step::Model, " , ", &mut draft).is_err(),
            "no model named"
        );
        assert!(matches!(
            step(Step::Model, "acme-1, acme-2 , acme-1", &mut draft),
            Ok(ProviderNext::Ask(Step::Store))
        ));
        assert_eq!(
            draft.models,
            vec!["acme-1".to_owned(), "acme-2".to_owned()],
            "duplicates dropped, order kept, padding trimmed"
        );
        assert!(step(Step::Store, "vault", &mut draft).is_err());
        assert!(matches!(
            step(Step::Store, "file", &mut draft),
            Ok(ProviderNext::Ask(Step::Key))
        ));

        // A credential too short to redact safely is refused before it is
        // stored, so it cannot end up on the wire unredacted.
        assert!(step(Step::Key, "short", &mut draft).is_err());

        // Removal names a configured provider and is confirmed before it runs.
        assert!(step(Step::Remove, "nothere", &mut draft).is_err());
        assert!(matches!(
            step(Step::Remove, "myai", &mut draft),
            Ok(ProviderNext::Ask(Step::ConfirmRemove))
        ));
        assert!(matches!(
            step(Step::ConfirmRemove, "no", &mut draft),
            Ok(ProviderNext::Cancelled(_))
        ));

        // Only the key step is a secret, and only it keeps the answer verbatim.
        for probe in [
            Step::Pick,
            Step::Name,
            Step::Kind,
            Step::BaseUrl,
            Step::Model,
        ] {
            assert!(!probe.masked(), "{probe:?} was masked");
        }
        assert!(Step::Key.masked());

        // The pick list carries the actions under the providers, and offers
        // nothing to remove when nothing is configured.
        let rows = Step::Pick
            .rows(&providers, "myai", Some("myai"))
            .expect("a list");
        assert_eq!(rows[0].0, "myai");
        // The active one says so, so a provider that is merely not current does
        // not read as one that was removed.
        assert_eq!(rows[0].1, "in use");
        let rows = Step::Pick.rows(&providers, "other", None).expect("a list");
        assert!(rows[0].1.contains("switch"), "{:?}", rows[0]);

        // Switched but not restarted: the session still runs the old one, and
        // the row says which is which rather than letting the new choice look
        // like it did not take.
        let rows = Step::Pick
            .rows(&providers, "other", Some("myai"))
            .expect("a list");
        assert!(rows[0].1.contains("after a restart"), "{:?}", rows[0]);
        assert!(rows.iter().any(|(name, _)| name == "+new"));
        assert!(rows.iter().any(|(name, _)| name == "-remove"));
        let empty = Step::Pick.rows(&[], "", None).expect("a list");
        assert!(empty.iter().all(|(name, _)| name != "-remove"));
        assert!(
            Step::Name.rows(&providers, "myai", None).is_none(),
            "a name is typed"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_auth_wizard_routes_actions_and_lists_providers() {
        use tui::AuthStep as Step;

        let invocation = Invocation {
            debug: false,
            config: None,
            provider: None,
            model: None,
            workspace: PathBuf::from("."),
            command: Command::Tui,
            no_color: true,
            output: Some(Output::Human),
        };
        let mut emitter = Emitter::new(Output::Human);
        let providers = vec!["antigravity".to_owned(), "chatgpt".to_owned()];
        let mut draft = String::new();

        // An empty answer leaves the wizard without touching anything.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "  ",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Cancelled(_)
        ));

        // Picking login moves to the provider picker.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "login",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::LoginProvider)
        ));

        // Picking set moves to the provider picker.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "set",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::SetProvider)
        ));

        // Choosing a provider to set asks for its key.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::SetProvider,
                "antigravity",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::SetKey)
        ));
        assert_eq!(draft, "antigravity");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_manual_grant_login_returns_immediately_instead_of_blocking_for_a_paste() {
        use tui::AuthStep as Step;

        // Regression test: `LoginProvider` for a manual-grant preset (the
        // Anthropic Claude Code client) used to read the pasted code with a
        // blocking stdin call made deep inside `auth_login`. The TUI's own
        // raw-mode line editor already owns the keyboard at that point, so
        // the terminal never saw the paste and the wizard hung. The step
        // must instead return at once, leaving the paste to arrive as an
        // ordinary line on the next turn (`PasteCode`).
        let invocation = Invocation {
            debug: false,
            config: None,
            provider: None,
            model: None,
            workspace: PathBuf::from("."),
            command: Command::Tui,
            no_color: true,
            // JSON output, not Human: this must not depend on or trigger a
            // real browser launch to prove the fix.
            output: Some(Output::Json),
        };
        let mut emitter = Emitter::new(Output::Json);
        let mut draft = String::new();

        assert!(matches!(
            auth_step(
                &invocation,
                Step::LoginProvider,
                "claude-oauth",
                &mut draft,
                &[],
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::PasteCode)
        ));
        let (provider, verifier) = draft.split_once('\n').expect("provider and verifier");
        assert_eq!(provider, "claude-oauth");
        assert_eq!(verifier.len(), 43, "a 32-byte PKCE verifier, unpadded");
    }

    /// The catalog is metadata, so where it lives is the operator's choice and
    /// the default costs no unlock prompt.
    #[test]
    fn the_credential_catalog_store_is_configurable_and_defaults_to_a_file() {
        use arsy_kernel::config::{Config, Layer, CREDENTIAL_STORES, DEFAULT_CREDENTIAL_STORE};

        let directory = tempfile::tempdir().unwrap();
        let write = |body: &str| {
            let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
            let json = arsy_kernel::config::json_from_toml(body, &path).unwrap();
            std::fs::write(&path, json).unwrap();
            Config::load(&[(Layer::User, path)])
        };

        // Unset is the file, so an operator who never asked is not asked to
        // unlock anything to read a list of handles.
        assert_eq!(DEFAULT_CREDENTIAL_STORE, "file");
        assert_eq!(
            write("schema_version = 1\n").unwrap().credential_store(),
            "file"
        );

        // Either store can be chosen, and the choice is traceable like any
        // other configured value.
        for store in CREDENTIAL_STORES {
            let config = write(&format!(
                "schema_version = 1\n[credentials]\nstore = \"{store}\"\n"
            ))
            .unwrap();
            assert_eq!(config.credential_store(), *store);
            assert_eq!(
                config.explain(Some("credentials.store"))["values"]["credentials.store"]["value"],
                **store
            );
        }

        // A name that is neither is refused at load, not silently defaulted:
        // a typo must not quietly send credentials somewhere else.
        let error = write("schema_version = 1\n[credentials]\nstore = \"vault\"\n").unwrap_err();
        assert!(format!("{error}").contains("vault"), "{error}");

        // The two names match the `secret://` stores, so one vocabulary covers
        // both the handle and the catalog.
        assert_eq!(CREDENTIAL_STORES, ["file", "os"]);
    }

    /// `/settings` and `/auth` print to a reader, not to a parser: the machine
    /// record is still the one JSON mode emits.
    #[test]
    fn configuration_and_credentials_render_as_rows_for_a_reader() {
        let report = json!({
            "schema_version": 1,
            "diagnostics": [],
            "values": {
                "provider.default": {"layer": "user", "path": "/cfg/arsy.json", "value": "myai"},
                "provider.endpoint.myai.kind": {
                    "layer": "workspace", "path": "/ws/.arsy/arsy.json", "value": "openai"
                },
            },
        });

        let rendered = human_config(&report, None);
        let listing = rendered["configuration"].as_str().expect("one string");
        assert!(listing.starts_with("2 values set"), "{listing}");
        assert!(
            listing.contains("default provider: myai  [user]"),
            "{listing}"
        );
        assert!(listing.contains("kind:  openai  [workspace]"), "{listing}");
        // Each source file is named once, under the rows, rather than repeated
        // on every one of them.
        assert_eq!(listing.matches("/cfg/arsy.json").count(), 1, "{listing}");
        assert!(listing.contains("from /ws/.arsy/arsy.json"), "{listing}");

        // Nothing set is a sentence, not an empty object.
        let empty = human_config(&json!({"values": {}}), Some("provider.default"));
        let listing = empty["configuration"].as_str().expect("one string");
        assert!(listing.contains("provider.default"), "{listing}");
        assert!(listing.contains("No configuration"), "{listing}");

        // Credentials list handles, never values, so a row is safe to show.
        let records = vec![
            AuthRecord {
                provider: "myai".to_owned(),
                handle: SecretHandle::new("os", "myai").unwrap(),
                created_at: 1,
                last_used: None,
                kind: CredentialKind::ApiKey,
            },
            AuthRecord {
                provider: "acme".to_owned(),
                handle: SecretHandle::new("file", "acme.key").unwrap(),
                created_at: 2,
                last_used: Some(9),
                kind: CredentialKind::OAuth,
            },
        ];
        let rendered = human_credentials(&records);
        let listing = rendered["credentials"].as_str().expect("one string");
        assert!(listing.starts_with("2 credentials stored"), "{listing}");
        assert!(listing.contains("secret://os/myai"), "{listing}");
        assert!(
            listing.contains("api_key  provider myai  · never used"),
            "{listing}"
        );
        // The kind column is padded, so what follows it lines up.
        assert!(listing.contains("oauth    provider acme"), "{listing}");
        assert_eq!(
            listing.matches("never used").count(),
            1,
            "only the unused credential carries the marker: {listing}"
        );

        let empty = human_credentials(&[]);
        assert!(
            empty["credentials"]
                .as_str()
                .expect("one string")
                .contains("auth set"),
            "an empty list must say how to add one"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_effort_picker_takes_a_number_a_name_or_the_current_setting() {
        // The list is numbered the way the model list is, and `off` is a row on
        // it rather than a word only a typist knows about.
        assert_eq!(
            tui::effort_choices(),
            vec![
                Some(Effort::Low),
                Some(Effort::Medium),
                Some(Effort::High),
                None
            ]
        );

        for (index, expected) in tui::effort_choices().iter().enumerate() {
            let answer = (index + 1).to_string();
            assert_eq!(
                tui::resolve_effort_answer(&answer, None).unwrap(),
                *expected,
                "row {answer}"
            );
        }

        for level in Effort::ALL {
            assert_eq!(
                tui::resolve_effort_answer(level.as_str(), None).unwrap(),
                Some(level)
            );
        }
        for word in ["off", "none", "unset"] {
            assert_eq!(
                tui::resolve_effort_answer(word, Some(Effort::High)).unwrap(),
                None,
                "{word} did not clear the level"
            );
        }

        // An empty line keeps what is set, so leaving the picker alone is not a
        // way to lose the setting.
        assert_eq!(
            tui::resolve_effort_answer("   ", Some(Effort::Medium)).unwrap(),
            Some(Effort::Medium)
        );

        // A rejected answer says why and changes nothing; the caller keeps the
        // picker open on it.
        for answer in ["hihg", "0", "5", "-1"] {
            assert!(
                tui::resolve_effort_answer(answer, Some(Effort::High)).is_err(),
                "{answer} was accepted"
            );
        }

        assert!(effort_line(Some(Effort::High)).contains("high"));
        assert!(effort_line(None).contains("no reasoning setting"));

        // The picker opens marked at what is set, so the first row a reader
        // sees marked is the answer they already have.
        assert_eq!(tui::effort_row(Some(Effort::Low)), 0);
        assert_eq!(tui::effort_row(Some(Effort::High)), 2);
        assert_eq!(tui::effort_row(None), 3, "an unset level marks `off`");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_effort_rows_are_arrowed_by_the_composer_that_already_owns_the_keys() {
        let mut composer = tui::Composer::default();
        composer.set_picking(true);
        composer.offer_table(Some(tui::EFFORT_ROWS), tui::effort_row(None));

        // Offered rows beat the command table, so a picker is not answered with
        // slash commands, and Up/Down move the mark rather than walk history.
        assert_eq!(composer.menu().len(), tui::EFFORT_ROWS.len());
        assert_eq!(composer.marked().as_deref(), Some("off"));
        composer.press(tui::Key::Down);
        assert_eq!(
            composer.marked().as_deref(),
            Some("low"),
            "the last row wraps"
        );
        composer.press(tui::Key::Up);
        assert_eq!(composer.marked().as_deref(), Some("off"));

        // Enter takes the marked level into the line; a second Enter sends it,
        // and what it sends is an answer the picker accepts.
        assert_eq!(composer.press(tui::Key::Enter), tui::Action::Redraw);
        assert_eq!(
            composer.press(tui::Key::Enter),
            tui::Action::Submit("off".to_owned())
        );
        assert_eq!(
            tui::resolve_effort_answer("off", Some(Effort::High)),
            Ok(None)
        );

        // Typing narrows the offered rows the way it narrows the commands.
        let mut composer = tui::Composer::default();
        composer.offer_table(Some(tui::EFFORT_ROWS), 0);
        for character in "me".chars() {
            composer.press(tui::Key::Char(character));
        }
        assert_eq!(composer.menu().len(), 1);
        assert_eq!(composer.marked().as_deref(), Some("medium"));

        // Clearing the offer hands the menu back to the command table.
        composer.offer_table(None, 0);
        assert!(composer.menu().is_empty(), "a task line offers no menu");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_branch_comes_from_head_including_a_worktree_pointer() {
        let root = std::env::temp_dir().join(format!("arsy-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(tui::branch(&root), None, "no checkout, no branch");

        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/slash-menu\n").unwrap();
        assert_eq!(tui::branch(&repo).as_deref(), Some("feat/slash-menu"));

        // Detached: HEAD holds the commit id, so the row shows a short one.
        std::fs::write(repo.join(".git/HEAD"), "3cd02230f0f0f0f0f0f0\n").unwrap();
        assert_eq!(tui::branch(&repo).as_deref(), Some("3cd02230"));

        // A worktree or submodule leaves a `gitdir:` pointer where the
        // directory would be.
        let linked = root.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: ../repo/.git\n").unwrap();
        assert_eq!(tui::branch(&linked).as_deref(), Some("3cd02230"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_menu_and_the_dispatch_table_hold_the_same_commands() {
        // `/model`, `/effort`, `/theme`, `/help`, and `/quit` are answered by
        // the loop itself; every other offered command must be an inspection it
        // knows how to run.
        for (name, _) in tui::COMMANDS {
            let handled = matches!(
                *name,
                "/model"
                    | "/effort"
                    | "/theme"
                    | "/provider"
                    | "/help"
                    | "/quit"
                    | "/new"
                    | "/clear"
                    | "/resume"
                    | "/update"
                    | "/rename"
                    | "/session"
                    | "/approval"
                    | "/plan"
                    | "/todo"
            ) || INSPECTIONS.iter().any(|(slash, _, _)| slash == name);
            assert!(handled, "{name} is offered but never dispatched");
        }
        for (slash, _, _) in INSPECTIONS {
            assert!(
                tui::COMMANDS.iter().any(|(name, _)| name == slash),
                "{slash} is dispatched but never offered"
            );
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn plan_subcommands_are_whole_words_and_everything_else_is_the_task_to_plan() {
        assert_eq!(plan_command("/plan"), PlanCommand::Enter(None));
        assert_eq!(
            plan_command("/plan add a cache to the resolver"),
            PlanCommand::Enter(Some("add a cache to the resolver".to_owned()))
        );
        assert_eq!(plan_command("/plan revise"), PlanCommand::Revise(None));
        assert_eq!(
            plan_command("/plan revise keep the existing schema"),
            PlanCommand::Revise(Some("keep the existing schema".to_owned()))
        );
        assert_eq!(plan_command("/plan approve"), PlanCommand::Approve);
        assert_eq!(plan_command("/plan cancel"), PlanCommand::Cancel);
        // Viewing the plan is not entering Plan Mode: `show` must not queue
        // "show" as a task to plan, and `list` is the same question.
        assert_eq!(plan_command("/plan show"), PlanCommand::Show);
        assert_eq!(plan_command("/plan list"), PlanCommand::Show);
        // A subcommand with anything after it is still that subcommand, not a
        // new planning task that silently re-enters Plan Mode.
        assert_eq!(plan_command("/plan approve please"), PlanCommand::Approve);
        assert_eq!(plan_command("/plan cancel for now"), PlanCommand::Cancel);
        // A word that only starts with one is not one.
        assert_eq!(
            plan_command("/plan approvals for the release"),
            PlanCommand::Enter(Some("approvals for the release".to_owned()))
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_plan_lifecycle_moves_between_planning_and_the_edit_mode_it_approves_into() {
        let approval = approval::ApprovalCell::new(approval::ApprovalMode::Default);
        let mut state = tui::TuiState::new("/workspace".into(), SessionId::new());

        // Entering plans rather than executes: the runtime built for the next
        // turn refuses every mutation.
        approval.enter_plan();
        state.set_approval_mode(approval.get().label());
        assert_eq!(
            approval.get().execution_mode(),
            arsy_code::agent::ExecutionMode::Plan
        );
        assert!(state.status_row(80, false, None).contains("PLAN"));

        // Revising stays in Plan Mode.
        let revision = revise_instruction(Some("reuse the existing cache"));
        assert!(revision.contains("reuse the existing cache"));
        assert!(revision.contains("without making changes"));
        assert_eq!(approval.get(), approval::ApprovalMode::Plan);
        assert_eq!(
            approval.get().execution_mode(),
            arsy_code::agent::ExecutionMode::Plan
        );

        // Approving leaves it for an existing edit-capable mode.
        let mode = approval.approve_plan();
        state.set_approval_mode(mode.label());
        assert_eq!(mode, approval::ApprovalMode::AcceptEdits);
        assert_eq!(
            approval.get().execution_mode(),
            arsy_code::agent::ExecutionMode::Normal
        );
        assert!(!state.status_row(80, false, None).contains("PLAN"));
        assert_eq!(
            approval::decide(approval.get(), "fs.write"),
            approval::Decision::Approve
        );
    }

    /// Reasoning is framed apart from the answer it precedes, so the box has
    /// to be finished before the answer's header opens. ARSY drew the header
    /// first, which left it between the box's last line and its bottom border.
    #[cfg(feature = "tui")]
    #[test]
    fn an_answer_closes_the_reasoning_box_before_it_opens_its_own() {
        let mut live = Streaming::default();
        let mut composer = tui::Composer::default();
        let mut screen: Vec<u8> = Vec::new();

        live.reason(
            &mut screen,
            &mut composer,
            false,
            "",
            "status",
            "weighing it up\n",
        )
        .unwrap();
        live.answer(
            &mut screen,
            &mut composer,
            false,
            "",
            "status",
            "the answer\n",
        )
        .unwrap();

        let drawn = String::from_utf8(screen).unwrap();
        let closed = drawn.find('╰').expect("the reasoning box is closed");
        let header = drawn.find("Response").expect("the answer announces itself");
        let prose = drawn.find("the answer").expect("the answer is drawn");
        assert!(
            closed < header,
            "the box closes before the header:\n{drawn}"
        );
        assert!(
            header < prose,
            "the header comes before the prose:\n{drawn}"
        );
    }

    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn a_follow_up_typed_during_a_provider_turn_is_carried_to_the_next_one() {
        use std::time::Duration;
        let route = tui::ModelRoute {
            provider: tui::CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        };
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (sender, keys) = std::sync::mpsc::channel();
        let typist = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            for byte in b"next thing\r" {
                let _ = sender.send(*byte);
            }
            // The keyboard outlives the turn, so the loop never reads the
            // follow-up as the operator hanging up.
            std::thread::sleep(Duration::from_secs(1));
        });
        // Streams for a moment, so there is a turn to type into, then ends.
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "printf '%s\\n' '{\"type\":\"turn.started\"}'; sleep 0.4; \
             printf '%s\\n' '{\"type\":\"turn.completed\"}'",
        ]);
        let result = drive_provider(
            command,
            "task\n",
            &route,
            &approval,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        typist.join().unwrap();

        // Reported as queued and actually queued: a row that says a follow-up
        // was taken, over a queue that dropped it, is worse than refusing it.
        assert_eq!(
            result.queued.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["next thing"],
            "the follow-up runs after this turn"
        );
        assert!(
            !result.interrupted,
            "typing a follow-up does not stop the turn"
        );
    }

    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn interactive_provider_cancellation_and_terminal_failures_are_bounded() {
        use std::time::{Duration, Instant};
        let route = tui::ModelRoute {
            provider: tui::CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        };
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (sender, keys) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            sender.send(0x1b).unwrap();
            // Keep input open until cancellation has had time to finish.
            std::thread::sleep(Duration::from_secs(3));
        });
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "trap '' TERM; while :; do printf '%s\\n' '{\"type\":\"turn.started\"}'; sleep 0.01; done"]);
        let started = Instant::now();
        let result = drive_provider(
            command,
            &"x".repeat(131_072),
            &route,
            &approval,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(result.interrupted);
        assert!(!result.quit, "Escape cancels the turn, not the session");
        assert!(started.elapsed() < Duration::from_secs(5));
        worker.join().unwrap();

        let (_sender, keys) = std::sync::mpsc::channel();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "read task; printf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"test failure\"}}'"]);
        let result = drive_provider(
            command,
            "task\n",
            &route,
            &approval,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(
            result.provider_failed,
            "a zero process exit must not mask a failed turn"
        );
        assert!(
            result.failure.is_some(),
            "a failed turn is recorded as a failure, not a completion"
        );

        // A provider that reports its turn complete and then lingers must not
        // hold the session: the answer is on screen, so the clock stops with
        // the turn rather than with the process. The stub ignores TERM, which
        // is what makes the wait for it a hang instead of a pause.
        let (_sender, keys) = std::sync::mpsc::channel();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "read task; printf '%s\\n' '{\"type\":\"turn.completed\"}'; trap '' TERM; sleep 30",
        ]);
        let started = Instant::now();
        let result = drive_provider(
            command,
            "task\n",
            &route,
            &approval,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a completed turn waited on the provider process"
        );
        assert!(
            result.failure.is_none(),
            "stopping a lingering provider is not a failed turn: {:?}",
            result.failure
        );
        assert!(
            !result.interrupted,
            "the turn completed, it was not cancelled"
        );
    }
    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn external_provider_skips_a_repeated_successful_git_command() {
        let route = tui::ModelRoute {
            provider: tui::CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        };
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Auto));
        let (_sender, keys) = std::sync::mpsc::channel();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            r#"read task; printf '%s\n' '{"type":"item.completed","item":{"type":"command_execution","command":"git remote -v && git push -u origin feat/x","exit_code":0}}'; printf '%s\n' '{"type":"item.started","item":{"type":"command_execution","command":"git remote -v && git push -u origin feat/x"}}'; sleep 1"#,
        ]);

        let result = drive_provider(
            command,
            "task\n",
            &route,
            &approval,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();

        assert!(result.failure.is_none(), "{:?}", result.failure);
        assert!(!result.interrupted);
        assert!(result.response.contains("duplicate was skipped"));
    }

    /// The catalog is written by one version and read by the next, so a
    /// record from before logins existed has to keep working.
    #[test]
    fn an_older_credential_catalog_still_reads() {
        let old = r#"[{"provider":"anthropic","handle":"secret://os/anthropic","created_at":1,"last_used":null}]"#;
        let records: Vec<AuthRecord> = serde_json::from_str(old).unwrap();
        assert_eq!(records[0].provider, "anthropic");
        assert_eq!(
            records[0].kind,
            CredentialKind::ApiKey,
            "a record written before logins existed is an API key"
        );

        // Round-trips under the name the catalog actually stores.
        let written = serde_json::to_string(&[AuthRecord {
            kind: CredentialKind::OAuth,
            ..records[0].clone()
        }])
        .unwrap();
        assert!(written.contains(r#""kind":"oauth""#), "{written}");
        let back: Vec<AuthRecord> = serde_json::from_str(&written).unwrap();
        assert_eq!(back[0].kind, CredentialKind::OAuth);

        let derived = written.replace(r#""kind":"oauth""#, r#""kind":"o_auth""#);
        let back: Vec<AuthRecord> = serde_json::from_str(&derived).unwrap();
        assert_eq!(
            back[0].kind,
            CredentialKind::OAuth,
            "a catalog written under the derived name must not read as corrupt"
        );
    }

    #[test]
    fn auth_entry_points_parse_without_accepting_a_secret_argument() {
        assert!(matches!(
            parse(["auth", "set", "anthropic"].map(str::to_owned))
                .unwrap()
                .command,
            Command::AuthSet { .. }
        ));
        assert!(matches!(
            parse(["auth", "list"].map(str::to_owned)).unwrap().command,
            Command::AuthList
        ));
        assert!(matches!(
            parse(["auth", "remove", "secret://os/anthropic"].map(str::to_owned))
                .unwrap()
                .command,
            Command::AuthRemove { .. }
        ));
        assert!(parse(["auth", "set", "anthropic", "raw-secret"].map(str::to_owned)).is_err());
    }

    /// The documented global flags apply before or after a subcommand, and
    /// compose with that subcommand's own flags.
    #[test]
    fn global_flags_apply_on_either_side_of_the_subcommand() {
        let before = parse(
            [
                "--config",
                "/tmp/session.toml",
                "--provider",
                "anthropic",
                "--model",
                "claude-opus-5",
                "run",
                "a task",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            before.config.as_deref(),
            Some(Path::new("/tmp/session.toml"))
        );
        assert_eq!(before.provider.as_deref(), Some("anthropic"));
        assert_eq!(before.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            before.command,
            Command::Run {
                task: "a task".to_owned(),
                image: None,
            }
        );

        let after = parse(
            [
                "run",
                "a task",
                "--provider",
                "anthropic",
                "--model",
                "claude-opus-5",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(after.provider, before.provider);
        assert_eq!(after.model, before.model);
        assert_eq!(after.command, before.command);

        // `--provider` is one flag with two readers: the global override, and
        // the filter `model list` applies. Both see the same value.
        let listing =
            parse(["model", "list", "--provider", "anthropic"].map(str::to_owned)).unwrap();
        assert_eq!(listing.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            listing.command,
            Command::ModelList {
                provider: Some("anthropic".to_owned()),
                capability: None,
            }
        );

        // A value flag with nothing after it is a usage error, not a silent
        // consumption of the next word.
        assert!(parse(["run", "a task", "--model"].map(str::to_owned)).is_err());
        assert!(parse(["--config"].map(str::to_owned)).is_err());

        // Tracing is opt-in, so an ordinary run stays as quiet as it was.
        assert!(!parse(["run", "a task"].map(str::to_owned)).unwrap().debug);
        assert!(
            parse(["run", "a task", "--debug"].map(str::to_owned))
                .unwrap()
                .debug
        );
    }

    /// A trace is diagnostics: it belongs on stderr, in one format whichever
    /// output mode is selected, and it must not exist at all when off.
    #[test]
    fn the_debug_trace_is_json_on_stderr_and_absent_unless_asked_for() {
        let mut quiet = Emitter::new(Output::Json);
        quiet.trace("request", json!({"round": 0}));
        assert_eq!(
            quiet.sequence, 0,
            "an untraced emitter does not even advance its sequence"
        );

        let mut traced = Emitter::new(Output::Json).with_debug(true);
        traced.trace("request", json!({"round": 0}));
        assert_eq!(traced.sequence, 1);
    }

    /// A model Claude Code or Codex is set to use fills in only for an endpoint
    /// arsy.json leaves without one, and never outranks one it names.
    #[test]
    fn another_tools_model_is_used_only_where_arsy_names_none() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        write_config_file(
            &path,
            "schema_version = 1\n\n[provider.endpoint.claude]\nkind = \"anthropic\"\n\
             base_url = \"https://api.anthropic.com\"\n",
        );
        let seed = arsy_kernel::config::CompatSeed {
            label: "claude".to_owned(),
            models: vec![arsy_kernel::config::ModelHint {
                model: "claude-opus-4-1".to_owned(),
                dialects: vec![arsy_kernel::config::Dialect::Anthropic],
                provider: None,
            }],
            ..Default::default()
        };
        let layers = [(arsy_kernel::config::Layer::User, path.clone())];
        let config = Config::load_with(&layers, std::slice::from_ref(&seed)).unwrap();
        let endpoint = config.endpoint(None).unwrap().clone();
        assert_eq!(
            selected_model(&config, &endpoint, None).unwrap(),
            "claude-opus-4-1"
        );

        write_config_file(
            &path,
            "schema_version = 1\n\n[provider.endpoint.claude]\nkind = \"anthropic\"\n\
             base_url = \"https://api.anthropic.com\"\nmodel = \"claude-sonnet-5\"\n",
        );
        let config = Config::load_with(&layers, &[seed]).unwrap();
        let endpoint = config.endpoint(None).unwrap().clone();
        assert_eq!(
            selected_model(&config, &endpoint, None).unwrap(),
            "claude-sonnet-5"
        );
    }

    /// `--model` chooses between what policy permits; it cannot reach past it.
    #[test]
    fn a_model_the_ceiling_excludes_is_refused_rather_than_dispatched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        write_config_file(
            &path,
            "schema_version = 1\n\n[provider.endpoint.local]\nkind = \"openai\"\nmodel = \"m1\"\n\
             models = [\"m1\", \"m2\"]\n\n[model]\nallowed = [\"m1\"]\n",
        );
        let config = Config::load(&[(arsy_kernel::config::Layer::User, path)]).unwrap();
        let endpoint = config.endpoint(None).unwrap().clone();

        assert_eq!(selected_model(&config, &endpoint, None).unwrap(), "m1");
        assert_eq!(
            selected_model(&config, &endpoint, Some("m1")).unwrap(),
            "m1"
        );
        let refused = selected_model(&config, &endpoint, Some("m2")).unwrap_err();
        assert_eq!(refused.code, ARSY_PRV_1000);
        assert!(
            refused.message.contains("m2"),
            "the refusal names the model that was asked for: {}",
            refused.message
        );
    }

    /// `--config` is applied last, so it wins an ordinary value — and loses
    /// every ceiling, which intersects rather than replaces.
    #[test]
    fn an_extra_config_file_overrides_a_value_but_cannot_widen_a_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("repo");
        std::fs::create_dir_all(workspace.join(".arsy")).unwrap();
        write_config_file(
            &workspace
                .join(".arsy")
                .join(arsy_kernel::config::CONFIG_FILE),
            "schema_version = 1\n\n[model]\ndefault = \"m1\"\nallowed = [\"m1\"]\n",
        );
        let extra = directory.path().join("session.json");
        write_config_file(
            &extra,
            "schema_version = 1\n\n[model]\ndefault = \"m9\"\nallowed = [\"m1\", \"m9\"]\n",
        );

        let config = load_config(&workspace, &workspace, Some(&extra)).unwrap();
        assert_eq!(config.model_default(), Some("m9"), "a later layer wins");
        assert!(
            !config.model_is_allowed("m9"),
            "a ceiling only ever narrows, whichever layer wrote it"
        );

        // A path the operator typed and got wrong is a diagnostic, not an
        // absent optional file.
        let missing = directory.path().join("absent.toml");
        assert_eq!(
            load_config(&workspace, &workspace, Some(&missing))
                .unwrap_err()
                .code,
            ARSY_CFG_1000
        );
    }

    /// A resumed session continues the conversation the model actually had —
    /// tool calls and results included — and does not replay a turn that was
    /// interrupted before it answered.
    #[cfg(feature = "tui")]
    #[test]
    fn resuming_restores_the_exchange_and_skips_a_turn_that_never_finished() {
        let workspace = std::env::temp_dir().join(format!("arsy-resume-{}", SessionId::new()));
        std::fs::create_dir_all(&workspace).unwrap();
        let session = SessionId::new();
        let store = open_store(&workspace).unwrap();
        let service =
            AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session).unwrap();

        let start = |prompt: &str| {
            service
                .start_turn(
                    Principal::System,
                    &ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
                        session,
                        prompt: prompt.to_owned(),
                        extensions: Extensions::new(),
                    })),
                )
                .unwrap()
        };

        // A turn that ran a tool and answered.
        let answered = start("fix the failing test");
        let exchange = vec![
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "fix the failing test".to_owned(),
                }],
            },
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::ToolCall {
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "cargo test"}),
                }],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::ToolResult {
                    id: "call-1".to_owned(),
                    content: "1 failed\n\nevidence: art-1".to_owned(),
                    is_error: true,
                }],
            },
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text {
                    text: "the assertion is inverted".to_owned(),
                }],
            },
        ];
        service
            .record_transcript(
                Principal::System,
                answered.turn,
                &transcript::persistable(&exchange),
            )
            .unwrap();
        service
            .complete_turn(Principal::System, answered.turn, &json!({}))
            .unwrap();

        // A turn the operator stopped. It wrote no transcript, so it
        // contributes nothing — not even the question it was asked.
        let stopped = start("and now rewrite the parser");
        service
            .fail_turn(
                Principal::System,
                stopped.turn,
                "user_interrupt",
                "stopped by the operator",
            )
            .unwrap();

        let (restored, cited) = reconstruct_session_conversation(&workspace, session);
        assert_eq!(
            cited.citations.len(),
            1,
            "the restored prefix knows which event it came from, so a later \
             compaction of it can cite one"
        );
        assert_eq!(
            restored, exchange,
            "the model resumes with the exchange it had, tool traffic included"
        );
        assert!(
            !restored.iter().any(|message| message.content.iter().any(
                |content| matches!(content, ModelContent::Text { text } if text.contains("parser"))
            )),
            "an interrupted turn's prompt is not replayed as history"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// An attachment is bounded and typed before it can cost a recorded turn.
    #[test]
    fn an_attached_image_is_read_as_canonical_content_or_refused_with_the_reason() {
        let directory = tempfile::tempdir().unwrap();

        // One pixel of PNG. What matters is the bytes round-trip, not the
        // picture: the adapter re-frames whatever this produces.
        let png = directory.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        let ModelContent::Image { media_type, data } = read_image(&png).unwrap() else {
            panic!("an image path reads as image content");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(
            data,
            arsy_kernel::provider::base64(b"\x89PNG\r\n\x1a\n"),
            "the canonical form is standard base64, which every adapter re-frames"
        );

        // A type nothing in this family reads is refused by name.
        let other = directory.path().join("notes.txt");
        std::fs::write(&other, b"text").unwrap();
        let refused = read_image(&other).unwrap_err();
        assert!(refused.message.contains("txt"), "{}", refused.message);

        // Past the limit is refused before a session is opened.
        let huge = directory.path().join("huge.png");
        std::fs::write(
            &huge,
            vec![0u8; usize::try_from(MAX_IMAGE_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(read_image(&huge).unwrap_err().message.contains("limit"));

        assert!(read_image(&directory.path().join("absent.png")).is_err());
    }

    /// A price that nobody configured is unknown, and unknown is not zero.
    #[test]
    fn a_turn_is_charged_from_configured_pricing_or_reported_as_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        write_config_file(
            &path,
            "schema_version = 1\n\n\
             [provider.endpoint.anthropic]\nkind = \"anthropic\"\nmodel = \"opus\"\n\
             models = [\"opus\", \"haiku\"]\n\n\
             [provider.endpoint.anthropic.pricing.opus]\n\
             input_micros_per_million = 15000000\n\
             output_micros_per_million = 75000000\n",
        );
        let config = Config::load(&[(arsy_kernel::config::Layer::User, path)]).unwrap();
        let endpoint = config.endpoint(None).unwrap();

        // 1000 in, 500 out: 15000000/1e6 * 1000 + 75000000/1e6 * 500.
        assert_eq!(charge(Some(endpoint), "opus", 1_000, 500), Some(52_500));
        assert_eq!(
            charge(Some(endpoint), "haiku", 1_000, 500),
            None,
            "a model with no configured price is unknown, not free"
        );
        assert_eq!(
            charge(Some(endpoint), "haiku", 0, 0),
            Some(0),
            "a turn that spent nothing cost nothing at any price"
        );
        assert_eq!(charge(None, "opus", 1_000, 0), None);

        // Rounding is up, so a sub-micro charge is never lost to the floor.
        assert_eq!(charge(Some(endpoint), "opus", 1, 0), Some(15));
    }

    /// One unpriced turn makes the session total unknown rather than wrong.
    #[test]
    fn session_cost_totals_go_unknown_rather_than_understating_the_spend() {
        use arsy_kernel::projection::{ProjectionSet, UsageTotals};

        let session = SessionId::new();
        let empty = ProjectionSet::new(session);
        assert_eq!(
            empty.usage(),
            UsageTotals {
                input_tokens: 0,
                output_tokens: 0,
                cost_micros: Some(0),
            },
            "a session that has run nothing knows it has spent nothing"
        );

        let store: Arc<dyn EventStore> = Arc::new(arsy_kernel::event::MemoryEventStore::default());
        let service = AgentService::attach(Arc::clone(&store), session).unwrap();
        service
            .record_usage(
                Principal::System,
                UsageTotals {
                    input_tokens: 10,
                    output_tokens: 4,
                    cost_micros: Some(25),
                },
            )
            .unwrap();
        let history = AgentService::history(store.as_ref(), session).unwrap();
        assert_eq!(
            ProjectionSet::rebuild(session, &history)
                .unwrap()
                .usage()
                .cost_micros,
            Some(25)
        );

        service
            .record_usage(
                Principal::System,
                UsageTotals {
                    input_tokens: 7,
                    output_tokens: 1,
                    cost_micros: None,
                },
            )
            .unwrap();
        let history = AgentService::history(store.as_ref(), session).unwrap();
        let usage = ProjectionSet::rebuild(session, &history).unwrap().usage();
        assert_eq!(usage.input_tokens, 17, "tokens are still counted");
        assert_eq!(
            usage.cost_micros, None,
            "one unpriced turn makes the total unknown, not $0.000025"
        );
    }

    #[test]
    fn compatibility_explain_is_a_real_command() {
        assert_eq!(
            parse(["compat", "explain", "claude"].map(str::to_owned))
                .unwrap()
                .command,
            Command::CompatExplain {
                ecosystem: arsy_code::compat::Ecosystem::Claude
            }
        );
        assert!(parse(["compat", "explain", "unknown"].map(str::to_owned)).is_err());
    }

    #[test]
    fn minimal_cli_recovers_a_crashed_session_and_has_bounded_noninteractive_outcomes() {
        let workspace = std::env::temp_dir().join(format!("arsy-cli-{}", SessionId::new()));
        std::fs::create_dir(&workspace).unwrap();
        let session = SessionId::new();
        let store = open_store(&workspace).unwrap();
        let service = AgentService::attach(store.clone(), session).unwrap();
        let request = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session,
            prompt: "recover this exact task".to_owned(),
            extensions: Extensions::new(),
        }));
        service.start_turn(actor(), &request).unwrap();
        drop(service);

        let invocation = Invocation {
            debug: false,
            config: None,
            provider: None,
            model: None,
            workspace: workspace.clone(),
            output: Some(Output::Ci),
            no_color: false,
            command: Command::Resume {
                session,
                follow: false,
            },
        };
        assert_eq!(
            resume(&invocation, session, false, &mut Emitter::new(Output::Ci)),
            Ok(0)
        );

        let events = store.read(session, 1, 8).unwrap();
        assert_eq!(events.len(), 2, "resume appends; it never rewrites history");
        assert_eq!(events[0].kind, "turn.started");
        assert_eq!(events[1].kind, "turn.failed");
        let EventPayload::Inline { data } = &events[0].payload else {
            panic!("turn evidence must remain inline");
        };
        assert!(data.to_string().contains("recover this exact task"));

        assert_eq!(doctor(&invocation, false, &mut Emitter::new(Output::Ci)), 0);
        assert_eq!(Diagnostic::error(ARSY_PRV_1000, "", "").exit_code(), 5);
        assert_eq!(Diagnostic::error("ARSY-POL-1000", "", "").exit_code(), 3);
        assert_eq!(
            execute(
                &Invocation {
                    command: Command::Tui,
                    ..invocation
                },
                false,
                &mut Emitter::new(Output::Ci),
            )
            .unwrap_err()
            .exit_code(),
            2,
            "no TTY refuses instead of waiting for interactive input"
        );

        drop(store);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
