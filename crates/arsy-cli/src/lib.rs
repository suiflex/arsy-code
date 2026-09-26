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
//! | `ARSY-TRN-1000` | the turn's tool-round budget ran out or looped |
//! | `ARSY-PRV-1002` | an installed provider CLI failed |
//! | `ARSY-SBX-1000` | no sandbox worker is available on this build |
//! | `ARSY-PRV-1001` | no credential store is registered |
//! | `ARSY-UIX-1000` | interactive terminal input or output failed |

mod acp;
#[cfg(feature = "tui")]
mod approval;
mod code;
mod config_edit;
mod config_load;
mod connector;
mod eval;
mod evidence;
mod extensions;
mod integrations;
mod mcp;
mod memory;
#[cfg(feature = "tui")]
mod picker;
mod policy;
mod probelm;
mod progress;
pub mod provider;
mod review;
mod run;
mod serve;
mod session;
mod subagent;
mod telemetry;
mod transcript;
#[cfg(feature = "tui")]
pub mod tui;
mod turn;
mod updater;
mod verify;

use config_load::{bootstrap_user_config, replace_file};
pub(crate) use config_load::{load_config, selected_model};
#[cfg(feature = "tui")]
use picker::prompt::{
    answer_prompt, cancels_to_task, leave_picker, masked, offer_rows, open_palette, open_route,
    prompt_status, seed_providers, submitted, Leaving, Opened, Picker, Prompt, Restoring, TaskPass,
    Typing,
};
#[cfg(feature = "tui")]
use picker::remembered::{endpoint_models, saved_effort, saved_route};
#[cfg(feature = "tui")]
use picker::session::configured_providers;
#[cfg(feature = "tui")]
use picker::wizard::configured_default;
#[cfg(feature = "tui")]
use picker::wizard::write_config;
use run as run_mod;
use run::{
    doctor, graph_failed, merge, prepare_task, resume, TaskRun, CONTEXT_BUDGET_TOKENS, TASK_BUDGET,
    TASK_LEASE_MS,
};
use run_mod::run as run_command;
#[cfg(feature = "tui")]
use turn::modern_gap;

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
        CredentialStore, FileCredentialStore, Redactor, SecretBroker, SecretError, SecretHandle,
        WithdrawnOsStore, FILE_STORE_ID, OS_STORE_ID,
    },
    service::AgentService,
    sqlite::{Durability, SqliteEventStore},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Every session of one workspace shares this store.
const STORE_PATH: &str = ".arsy/sessions.sqlite3";

/// No provider credential is available, so the turn cannot dispatch.
pub const ARSY_PRV_1000: &str = "ARSY-PRV-1000";
/// The turn's tool-round budget ran out, or the model looped on one failing
/// call. The harness stopped the turn; the provider is not at fault.
pub const ARSY_TRN_1000: &str = "ARSY-TRN-1000";
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
  arsy update [--check] [--force] check or download the latest release for this OS

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
            "RET" | "CTX" | "PLN" | "MDL" | "TRN" => 9,
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
        force: bool,
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
    /// `arsy verify <SESSION>`: rebuild the completion proof and report it.
    Verify {
        session: SessionId,
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
    let Some(name) = parsed.name.clone() else {
        return Ok(invocation(global, Command::Tui));
    };
    // A subsystem reads the shared arguments and leaves them alone, so it is
    // tried first: what is left has to take `parsed` apart, and cannot hand
    // it back if the name turns out not to be its own.
    let command = match parse_subsystem(&name, &parsed) {
        Some(command) => command?,
        None => parse_owned(&name, parsed)?,
    };
    Ok(invocation(global, command))
}

/// What each subsystem's own parser is called.
///
/// A table rather than a match: every entry is a name and a function of the
/// same shape, so the list of subcommands is something to read rather than
/// control flow to follow.
type SubsystemParser = fn(&ParsedArguments) -> Result<Command, Diagnostic>;

const SUBSYSTEMS: &[(&str, SubsystemParser)] = &[
    ("verify", verify::parse),
    ("session", session::parse),
    ("artifact", evidence::parse_artifact),
    ("gc", evidence::parse_gc),
    ("migrate", session::parse_migrate),
    ("review", review::parse),
    ("code", code::parse),
    ("memory", memory::parse),
    ("policy", policy::parse),
    ("serve", serve::parse),
    ("skill", extensions::parse_skill),
    ("plugin", extensions::parse_plugin),
    ("provider", provider::parse_list),
    ("model", provider::parse_models),
    ("mcp", mcp::parse),
];

fn parse_subsystem(name: &str, parsed: &ParsedArguments) -> Option<Result<Command, Diagnostic>> {
    SUBSYSTEMS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, parse)| parse(parsed))
}

/// The subcommands that consume the arguments rather than read them.
///
/// Each takes `parsed` apart — a positional it turns into a task, a flag it
/// reads directly — which is why they cannot be entries in [`SUBSYSTEMS`].
fn parse_owned(name: &str, mut parsed: ParsedArguments) -> Result<Command, Diagnostic> {
    Ok(match name {
        "run" => Command::Run {
            task: only_argument(parsed.positional, "run", "<TASK>")?,
            image: parsed.image.take(),
        },
        "resume" => parse_resume(parsed.positional, parsed.follow)?,
        "doctor" => parse_doctor(parsed.positional, parsed.strict)?,
        "update" => Command::Update {
            check_only: parsed.check,
            force: parsed.force,
        },
        "eval" => Command::Eval {
            suite: PathBuf::from(only_argument(parsed.positional, "eval", "<SUITE>")?),
            trials: parsed.trials,
            strict: parsed.strict,
            out: parsed.out,
        },
        "compat" => Command::CompatExplain {
            ecosystem: compatibility_kind(parsed.positional)?,
        },
        "auth" => parse_auth(parsed.positional, parsed.handle, parsed.force)?,
        "config" => parse_config(parsed.positional)?,
        "hook" => integrations::parse("hook", parsed.positional, parsed.source, parsed.event)?,
        other => return Err(unknown_command(other)),
    })
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
pub(crate) struct Emitter {
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

    /// One line a server logged about itself.
    ///
    /// Not a diagnostic: nothing about ARSY went wrong, and a server that
    /// prints a deprecation notice on every start must not read as a warning
    /// the operator is meant to act on. It is the server's own untrusted text,
    /// carried through already scrubbed, and it goes where a trace goes — to
    /// stderr, so a pipeline reading stdout is unaffected.
    fn server_log(&mut self, message: &str) {
        match self.output {
            Output::Json => self.record("mcp_log", json!({"message": message})),
            Output::Acp => {}
            _ => {
                let _ = writeln!(io::stderr(), "  {}", terminal_text(message));
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
            auth_set(provider, handle.as_deref(), tty, emitter)
        }
        Command::AuthLogin { provider } => auth_login(invocation, provider, emitter),
        Command::AuthList => auth_list(emitter),
        Command::AuthRemove { handle, force } => auth_remove(handle, *force, emitter),
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
        Command::Verify { session } => verify::run(invocation, *session, emitter),
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
    // A blocked gate exits nonzero even when the report is otherwise fine:
    // that is what makes a safety regression stop a promotion rather than
    // appear as a line in a document nobody reads.
    let code = report.exit_code();
    emitter.result(serde_json::to_value(report).map_err(storage_failed)?);
    Ok(code)
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
        Command::Run { task, image } => run_command(invocation, task, image.as_deref(), emitter),
        Command::Resume { session, follow } => resume(invocation, *session, *follow, emitter),
        Command::Update { check_only, force } => execute_update(*check_only, *force, emitter),
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

fn execute_update(check_only: bool, force: bool, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    updater::execute_update(check_only, force, emitter)
}

/// A redactor that knows every non-interactive credential this workspace has
/// stored, installed on the emitter so anything it prints goes through the
/// same pipeline.
fn redactor(emitter: &mut Emitter) -> Result<Redactor, Diagnostic> {
    let mut broker = SecretBroker::new();
    // The withdrawn store is registered so a handle left over from an older
    // build is answered by the store it names. It opens nothing, and preloading
    // it costs no prompt, so the interactive path no longer has to skip it.
    broker.register_store(Box::new(WithdrawnOsStore));
    broker.register_store(Box::new(FileCredentialStore));
    for record in catalog()? {
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

/// Read every configuration layer for this workspace.
///
/// An invalid file is fatal rather than skipped: continuing with a partly
/// applied policy would silently run under something the operator never wrote.
/// Every discovered configuration layer, plus the one `--config` named.
///
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

/// Read the credential catalog.
///
/// The catalog is metadata — handles, provider names, timestamps — and never a
/// secret value. It lives in one place, beside the user configuration, owned by
/// the operator: there is no second store left to choose between.
fn read_catalog() -> Result<Option<String>, Diagnostic> {
    match FileCredentialStore.resolve(CATALOG_FILE) {
        Ok(raw) => Ok(Some(raw)),
        Err(SecretError::NotFound(_)) => Ok(None),
        Err(error) => Err(secret_failed(error)),
    }
}

fn write_catalog(raw: &str) -> Result<(), Diagnostic> {
    let path = FileCredentialStore::path(CATALOG_FILE)
        .ok_or_else(|| secret_failed("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(storage_failed)?;
    }
    // Created owner-only rather than created and then narrowed: a chmod after
    // the write leaves a window where the catalog is readable by the whole
    // machine.
    let mut file = owner_only(&path)?;
    file.write_all(format!("{raw}\n").as_bytes())
        .map_err(storage_failed)
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

fn catalog() -> Result<Vec<AuthRecord>, Diagnostic> {
    let Some(raw) = read_catalog()? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|_| secret_failed("credential catalog is corrupt"))
}

fn save_catalog(records: &[AuthRecord]) -> Result<(), Diagnostic> {
    let raw = serde_json::to_string(records).map_err(|error| secret_failed(error.to_string()))?;
    write_catalog(&raw)
}

fn auth_set(
    provider: &str,
    requested: Option<&str>,
    tty: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    // A requested name is taken as the file the credential lands in, so
    // `--name gateway.key` and a bare `--name gateway` both name one file.
    let requested = requested.unwrap_or(provider);
    let name = if requested.ends_with(".key") {
        requested.to_owned()
    } else {
        format!("{requested}.key")
    };
    let handle = SecretHandle::new(FILE_STORE_ID, &name).map_err(secret_failed)?;
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
    let store = FileCredentialStore;
    let previous = match store.resolve(&name) {
        Ok(value) => Some(value),
        Err(SecretError::NotFound(_)) => None,
        Err(error) => return Err(secret_failed(error)),
    };
    store.set(&name, &secret).map_err(secret_failed)?;
    let mut records = catalog()?;
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
    if let Err(error) = save_catalog(&records) {
        if let Some(previous) = previous {
            let _ = store.set(&name, &previous);
        } else {
            let _ = store.remove(&name);
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
        configured,
        preset,
        oauth,
        synthesize,
    })
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

/// Store the token set a login produced, under the same kind of handle an
/// API key would use, so everything downstream — resolution, redaction,
/// `auth remove` — treats the two the same.
///
/// The catalog is file-only: the OS keyring store was withdrawn, so a handle
/// that names it is re-pointed at the file the token just went into rather
/// than honoured.
fn store_oauth_login(
    _invocation: &Invocation,
    provider: &str,
    login: &OAuthLogin,
    tokens: arsy_kernel::oauth::TokenSet,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    // A configured handle is reused as configured, unless it names the
    // withdrawn keyring: signing in again is exactly the act that moves such a
    // credential, so this is where the move happens rather than a failure.
    let handle_name = match login
        .configured
        .as_ref()
        .and_then(|e| e.credential.as_ref())
    {
        Some(existing) if existing.store() == FILE_STORE_ID => existing.name().to_owned(),
        _ => format!("{provider}.key"),
    };
    let handle = SecretHandle::new(FILE_STORE_ID, &handle_name).map_err(secret_failed)?;
    let raw = serde_json::to_string(&tokens).map_err(|error| secret_failed(error.to_string()))?;
    FileCredentialStore
        .set(&handle_name, &raw)
        .map_err(secret_failed)?;
    let mut records = catalog()?;
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
    save_catalog(&records)?;

    // An endpoint still pointed at the withdrawn keyring is re-pointed at the
    // file the token just went into. Signing in again is what moves such a
    // credential, and a move that left the configuration behind would have
    // stored a token nothing reads.
    let stale_keyring = login
        .configured
        .as_ref()
        .and_then(|endpoint| endpoint.credential.as_ref())
        .is_some_and(|existing| existing.store() == OS_STORE_ID);
    if stale_keyring {
        // `write_config` owns the user file and only that one, so an endpoint
        // a repository or an enterprise layer defined is not ours to edit. Say
        // so rather than report a move that did not happen: the operator would
        // otherwise meet the same refusal next turn, told to run the command
        // they just ran.
        let mut repointed = false;
        write_config(|config| {
            match config_edit::set_existing(
                config,
                &["provider", "endpoint", provider],
                "credential",
                serde_json::Value::String(handle.to_string()),
            )? {
                Some(updated) => {
                    repointed = true;
                    Ok(updated)
                }
                None => Ok(config.to_owned()),
            }
        })
        .map_err(|error| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!(
                    "signed in, but `provider.endpoint.{provider}.credential` still names the \
                     keyring: {error}"
                ),
                format!("set it to `{handle}` by hand; the credential is already stored"),
            )
        })?;
        if !repointed {
            emitter.diagnostic(&Diagnostic::warning(
                ARSY_PRV_1000,
                format!(
                    "signed in, but `provider.endpoint.{provider}` is not defined in the user \
                     configuration, so its `credential` still names the withdrawn keyring"
                ),
                format!("set it to `{handle}` in the file `arsy config explain` names for it"),
            ));
        }
    }
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

fn auth_list(emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let records = catalog()?;
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
    handle: &SecretHandle,
    _force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    // A handle naming the withdrawn keyring can still be removed, because it
    // can still be listed: a catalog that names a handle no command can delete
    // is a catalog that only grows. Removing it drops the record and nothing
    // else — the keyring entry itself is the operator's to delete, and saying
    // otherwise would be a claim this build cannot make good on.
    if !matches!(handle.store(), OS_STORE_ID | FILE_STORE_ID) {
        return Err(secret_failed(format!(
            "no credential store `{}` to remove from",
            handle.store()
        )));
    }
    let original = catalog()?;
    let mut records = original.clone();
    records.retain(|record| &record.handle != handle);
    save_catalog(&records)?;
    if handle.store() == FILE_STORE_ID {
        if let Err(error) = FileCredentialStore.remove(handle.name()) {
            // The catalog is written first, so a failed delete has to put it
            // back rather than leave a stored credential nothing lists.
            let _ = save_catalog(&original);
            return Err(secret_failed(error));
        }
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

/// How long the mark's scan takes, and how often it draws a frame.
#[cfg(feature = "tui")]
const INTRO_MS: u64 = 800;
#[cfg(feature = "tui")]
const INTRO_TICK_MS: u64 = 33;

#[cfg(feature = "tui")]
fn draw_launch(
    stdout: &mut io::Stdout,
    state: &tui::TuiState,
    colour: bool,
    provider_available: bool,
) -> Result<(), Diagnostic> {
    let width = tui::terminal_width();
    let card = state.render(width, colour);
    scan_mark(stdout, state, width, colour, &card)?;
    writeln!(stdout, "{card}").map_err(terminal_failed)?;
    writeln!(
        stdout,
        "{}Use /help for commands, /mcp and /hooks to inspect integrations.",
        modern_gap(),
    )
    .map_err(terminal_failed)?;
    if tui::modern_style() {
        writeln!(stdout, "{}", state.approval_hint()).map_err(terminal_failed)?;
    }
    if !provider_available {
        writeln!(stdout, "Provider unavailable. Inspection is available; configure a `[provider.endpoint.<name>]` table and run `arsy auth set <name>`, or install Codex and run codex login, to execute tasks.").map_err(terminal_failed)?;
    }
    Ok(())
}

/// Sweep a lit band down the mark before the card settles.
///
/// The frames are painted over one another — one card, the cursor walked back
/// to its first line, the next card on top — so only the settled card is left
/// in scrollback for the terminal to scroll back to. Every frame is the same
/// height and reaches the same width, so a frame covers the one under it
/// without erasing anything first.
///
/// Skipped where nothing would see it, or where it would misbehave: without
/// colour there is no band to sweep, a redirected stdout would collect every
/// frame as text, and a window shorter than the card would scroll under the
/// cursor as it walked back.
#[cfg(feature = "tui")]
fn scan_mark(
    stdout: &mut io::Stdout,
    state: &tui::TuiState,
    width: usize,
    colour: bool,
    card: &str,
) -> Result<(), Diagnostic> {
    use std::io::IsTerminal;

    let height = card.lines().count();
    if !colour || !stdout.is_terminal() || tui::terminal_rows() <= height + 2 || height < 2 {
        return Ok(());
    }
    // A card too narrow to hold the mark drops it, and then there is nothing
    // to sweep and nothing worth making the operator wait for.
    if state.render_frame(width, colour, Some(0.5)) == card {
        return Ok(());
    }
    let frames = (INTRO_MS / INTRO_TICK_MS).max(1);
    let rewind = height - 1;
    write!(stdout, "\x1b[?25l").map_err(terminal_failed)?;
    for frame in 0..frames {
        let progress = frame as f32 / frames as f32;
        write!(
            stdout,
            "{}\r\x1b[{rewind}A",
            state.render_frame(width, colour, Some(progress))
        )
        .map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        std::thread::sleep(std::time::Duration::from_millis(INTRO_TICK_MS));
    }
    write!(stdout, "\x1b[?25h").map_err(terminal_failed)
}

#[cfg(feature = "tui")]
fn next_tui_action(
    queued: &mut std::collections::VecDeque<String>,
    context: ReadLineContext<'_>,
) -> Result<Option<tui::Action>, Diagnostic> {
    match queued.pop_front() {
        Some(line) => Ok(Some(tui::Action::Submit(line))),
        None => read_line(context),
    }
}

#[cfg(feature = "tui")]
fn cancel_picker(
    prompt: &Prompt,
    stdout: &mut io::Stdout,
    composer: &mut tui::Composer,
    leaving: Leaving<'_>,
) -> Result<(), Diagnostic> {
    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
    writeln!(stdout, "{}", leave_picker(prompt, leaving)).map_err(terminal_failed)
}

#[cfg(feature = "tui")]
fn run_tui(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let workspace = workspace_root(&invocation.workspace)?;
    let mut stdout = io::stdout();

    let Opened {
        native,
        native_requested,
        detected,
        mut provider_available,
    } = open_route(invocation, &workspace)?;
    let colour = !invocation.no_color && std::env::var_os("NO_COLOR").is_none();

    let (theme_config, mut theme) = open_palette(invocation, &workspace, emitter);
    let mut models = endpoint_models(invocation);
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
    // Opened here rather than at the first recorded turn, because a session
    // that has not run one — plan mode, a refused turn, an operator still
    // deciding — is still a session: /session and the status row should name
    // it, and a turn that comes later finds the store already there.
    let _store = open_store(&workspace)?;
    // The first drawing of the card, so the loop below does not read it as a
    // change and repaint over the notices printed under it.
    state.card_is_stale();
    draw_launch(&mut stdout, &state, colour, provider_available)?;

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
            writeln!(
                stdout,
                "{}{}",
                modern_gap(),
                state.render(tui::terminal_width(), colour)
            )
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
        );
        composer.set_masked(masked(&prompt));
        // While the theme picker is open, repaint in whichever theme is
        // arrowed onto so it can be seen before Enter takes it.
        let preview_theme = |name: &str| tui::set_palette(name, &theme_config.roles);
        let preview: Option<&dyn Fn(&str)> = match prompt {
            Prompt::Theme => Some(&preview_theme),
            _ => None,
        };
        let Some(input) = next_tui_action(
            &mut queued,
            ReadLineContext {
                keys: &keys,
                decoder: &mut decoder,
                composer: &mut composer,
                stdout: &mut stdout,
                colour,
                status: &status,
                preview,
                transcript: &mut transcript,
                state: &state,
            },
        )?
        else {
            if cancels_to_task(&prompt) {
                cancel_picker(
                    &prompt,
                    &mut stdout,
                    &mut composer,
                    Leaving {
                        effort,
                        theme: &theme,
                        roles: &theme_config.roles,
                        draft: &mut draft,
                        auth_draft: &mut auth_draft,
                        session: state.session_id(),
                        route: &mut route,
                    },
                )?;
                prompt = Prompt::Task;
                continue;
            }
            break;
        };
        // Shift+Tab changes the mode where it stands: it never becomes a line
        // for the prompt to answer. Every other action redraws and nothing
        // more.
        let Some(line) = submitted(
            input,
            &approval,
            &mut state,
            &mut transcript,
            &mut stdout,
            colour,
        ) else {
            continue;
        };
        let pass = answer_prompt(
            prompt,
            &line,
            invocation,
            Typing {
                workspace: &workspace,
                colour,
                provider_available: &mut provider_available,
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
pub(crate) struct ReadLineContext<'a> {
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
    let mut composer_height = tui::terminal_rows();
    composer.set_height(composer_height);
    let mut measured = std::time::Instant::now();
    loop {
        let refreshed = std::time::Instant::now();
        if measured.elapsed() >= std::time::Duration::from_millis(100) {
            let next_width = tui::terminal_width();
            let next_rows = tui::terminal_rows();
            // Rows count too: a taller terminal shows more of the command
            // menu, and a wider one reflows the transcript. Measuring both is
            // what makes a terminal dragged between sizes settle rather than
            // keep the shape it was opened with.
            if next_width != width || next_rows != composer_height {
                transcript
                    .repaint(stdout, next_width, colour, state)
                    .map_err(terminal_failed)?;
                composer.invalidate();
            }
            width = next_width;
            composer_height = next_rows;
            composer.set_height(next_rows);
            measured = std::time::Instant::now();
        }
        if let (Some(preview), Some(row)) = (preview, composer.highlighted()) {
            preview(&row);
        }
        write!(stdout, "{}", composer.render(width, colour, status)).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        match drain_input_keys(
            keys, decoder, composer, stdout, colour, width, transcript, state, refreshed,
        )? {
            Drain::Refresh => {}
            Drain::Answer(answer) => return Ok(answer),
        }
    }
}

/// What one pass of the key loop asked for.
enum Drain {
    /// Redraw and measure the terminal again.
    Refresh,
    /// The line ended one way or another; hand the action up.
    Answer(Option<tui::Action>),
}

/// Take the keys waiting, ending the line when one of them ends it.
///
/// A helper rather than the body of `read_line`'s inner loop, because the two
/// loops answer different questions — when to repaint, and what a key means —
/// and neither should have to hold the other's state.
#[allow(clippy::too_many_arguments)]
fn drain_input_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    stdout: &mut dyn Write,
    colour: bool,
    width: usize,
    transcript: &mut tui::Transcript,
    state: &tui::TuiState,
    refreshed: std::time::Instant,
) -> Result<Drain, Diagnostic> {
    loop {
        // The timeout is what tells a lone Escape apart from the start of an
        // arrow-key sequence.
        let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(byte) => decoder.feed(byte),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let key = decoder.flush_escape();
                if key.is_none() && refreshed.elapsed() >= std::time::Duration::from_millis(100) {
                    return Ok(Drain::Refresh);
                }
                key
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(Drain::Answer(None));
            }
        };
        let Some(key) = key else { continue };
        match composer.press(key) {
            tui::Action::Submit(line) => return Ok(Drain::Answer(Some(tui::Action::Submit(line)))),
            tui::Action::CycleMode => return Ok(Drain::Answer(Some(tui::Action::CycleMode))),
            tui::Action::Expand => {
                if transcript.toggle_last_tool() {
                    transcript
                        .repaint(stdout, width, colour, state)
                        .map_err(terminal_failed)?;
                }
                return Ok(Drain::Refresh);
            }
            tui::Action::Quit => return Ok(Drain::Answer(None)),
            tui::Action::Redraw => return Ok(Drain::Refresh),
            tui::Action::None => {}
        }
    }
}
/// has, and telling those apart is the whole job of `arsy resume`.
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
        disabled: config.hook_disabled().clone(),
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
///
/// `trace` takes the child's own events rather than the parent's emitter,
/// because a child runs on its own thread and the emitter writes to one
/// terminal. `cancel` is checked between rounds and between tool calls: those
/// are the points where stopping leaves the workspace in a state the child can
/// describe, which is what makes cancellation safe rather than merely fast.
#[allow(clippy::too_many_arguments)]
pub(crate) fn child_turn(
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    watch: &mut dyn FnMut(
        &arsy_kernel::observer::RedactedProjection,
    ) -> Option<arsy_kernel::observer::Intervention>,
    trace: &mut dyn FnMut(&str, Value),
    steer: &mut dyn FnMut() -> Vec<String>,
    boundary: &mut dyn FnMut() -> bool,
    cancel: &arsy_kernel::scheduler::CancelToken,
    tokens: &mut (u64, u64),
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
        if cancel.is_cancelled() || !boundary() {
            return Err(CHILD_CANCELLED.to_owned());
        }
        absorb_steering(&mut request.messages, steer(), trace);
        request.idempotency_key =
            IdempotencyKey::new(format!("{base}-{round}")).map_err(|error| error.to_string())?;
        answer.clear();
        let stream =
            arsy_kernel::provider::stream_with_retry(provider, &request, &mut std::thread::sleep)
                .map_err(|error| error.to_string())?;
        let calls = absorb_child_stream(stream, round, &mut answer, tokens, trace)?;
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
            // Between calls, not mid-call: a tool that has started is allowed
            // to finish and say what it did, so a cancelled child still
            // reports the effects it already had.
            if cancel.is_cancelled() || !boundary() {
                return Err(CHILD_CANCELLED.to_owned());
            }
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
                trace("subagent.stopped", json!({"reason": reason}));
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

/// Read one round of a child's stream into its answer, its usage, and the
/// calls it asked for.
///
/// The same split the parent's loop already has: what a stream said is one
/// question, and what to do about it is another.
fn absorb_child_stream(
    stream: arsy_kernel::provider::ModelEventStream,
    round: usize,
    answer: &mut String,
    tokens: &mut (u64, u64),
    trace: &mut dyn FnMut(&str, Value),
) -> Result<Vec<(String, String, Value)>, String> {
    let mut calls = Vec::new();
    for event in stream {
        match event.map_err(|error| error.to_string())? {
            ModelEvent::TextDelta { text } => answer.push_str(&text),
            ModelEvent::ToolCallCompleted {
                id,
                name,
                arguments,
                ..
            } => {
                trace(
                    "model.tool_call",
                    json!({"round": round, "id": id, "name": name, "arguments": arguments}),
                );
                calls.push((id, name, arguments));
            }
            ModelEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                // Counted here rather than inferred by the parent: what a
                // child spent has to settle against the budget its task
                // reserved, and only the child sees its own stream.
                tokens.0 = tokens.0.saturating_add(input_tokens);
                tokens.1 = tokens.1.saturating_add(output_tokens);
            }
            _ => {}
        }
    }
    Ok(calls)
}

/// Put what the parent said into the child's next round.
///
/// A round boundary is where a child can absorb a new instruction without
/// abandoning work in progress. Messages narrow what it was already asked to
/// do; they cannot widen what it may do, because its grants were fixed when
/// the graph created it — there is nothing in a message to widen them with.
fn absorb_steering(
    messages: &mut Vec<ModelMessage>,
    instructions: Vec<String>,
    trace: &mut dyn FnMut(&str, Value),
) {
    for instruction in instructions {
        trace("subagent.steered", json!({"instruction": &instruction}));
        messages.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text { text: instruction }],
        });
    }
}

/// What a child says when it was asked to stop. A reason rather than a
/// silence, so the attempt's terminal record is explicable.
pub(crate) const CHILD_CANCELLED: &str = "the subagent was cancelled before it answered";

/// What a turn cost, when configuration says what its model charges.
///
/// `None` means nobody wrote a price down. Reported as unknown rather than as
/// zero, because a running total that silently treats every unpriced turn as
/// free is worse than one that admits the gap: the first is wrong and looks
/// right, the second is right about what it does not know.
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
    // The task and attempt this turn runs under, when it has one. Evidence the
    // turn records — a validation, above all — is attributed to them, so a
    // later reader can tell which try of which task it belongs to.
    lineage: Option<(TaskId, Option<arsy_kernel::domain::AttemptId>)>,
    connector: Option<&connector::McpConnector>,
    emitter: &mut Emitter,
    skills: &[arsy_code::agent::instructions::Skill],
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
                        task: lineage.map(|(task, _)| task),
                        attempt: lineage.and_then(|(_, attempt)| attempt),
                    })
                })
                .transpose()?,
            mcp: connections,
            mcp_pending: pending,
        },
        skills,
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
    show_mcp_logs(connector.logs(), config.mcp_log(), emitter);
    let (connections, pending) = connector.session();
    (Some(connections), pending, discovered)
}

/// Show what the servers logged since the last turn boundary, as much of it as
/// `ui.mcp_log` asks for.
///
/// Drained here rather than written by the forwarding threads: those run while
/// a frame is being painted, and a write from one lands wherever the cursor
/// happens to be. This is a point in the loop where nothing else is drawing.
///
/// The lines are a server's own output — untrusted content — so they are shown
/// as notes and never as diagnostics an operator could mistake for ARSY's.
fn show_mcp_logs(logs: Vec<(String, String)>, level: &str, emitter: &mut Emitter) {
    for line in mcp_log_rows(logs, level) {
        emitter.server_log(&line);
    }
}

/// What `ui.mcp_log` leaves of a turn's server logging.
///
/// Separate from the writing so the level and the counting can be tested
/// without a terminal: this is the part that can be wrong.
fn mcp_log_rows(logs: Vec<(String, String)>, level: &str) -> Vec<String> {
    if logs.is_empty() || level == "hidden" {
        return Vec::new();
    }
    if level == "full" {
        return logs
            .into_iter()
            .map(|(server, line)| format!("mcp {server}: {line}"))
            .collect();
    }
    // `summary`: one line for the turn, because a workspace with four servers
    // that each say something on connect would otherwise spend four rows of
    // the transcript saying nothing an operator can act on. The map holds
    // each server's slot so counting stays linear in the logs, not quadratic
    // in servers × lines.
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (server, _) in logs {
        if let Some(&index) = slots.get(server.as_str()) {
            counts[index].1 += 1;
        } else {
            let index = counts.len();
            counts.push((server, 1));
            slots.insert(counts[index].0.clone(), index);
        }
    }
    let lines: usize = counts.iter().map(|(_, count)| count).sum();
    let word = if lines == 1 { "line" } else { "lines" };
    let what = match counts.as_slice() {
        [(server, _)] => server.clone(),
        servers => format!("{} servers", servers.len()),
    };
    vec![format!(
        "mcp {what} · {lines} log {word} · `ui.mcp_log = \"full\"` to read them"
    )]
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
        &prompt_skills(root, config),
        memory::recalled(root, MAX_RECALLED_MEMORY_BYTES).as_deref(),
        mode,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .ok()?;
    Some(arsy_code::agent::instructions::render(&compiled))
}

/// Every declared skill the operator has not switched off, with the
/// description its front matter declares.
///
/// Off beats discovered: a skill in `skill.disabled` is still listed by
/// `arsy skill list`, which is where an operator goes to find out what they
/// switched off, but the model is not told about it at all.
fn prompt_skills(
    root: &Path,
    config: &arsy_kernel::config::Config,
) -> Vec<arsy_code::agent::instructions::Skill> {
    use arsy_code::agent::instructions::Skill;
    use arsy_code::compat::Ecosystem;
    let importer = arsy_code::compat::CompatibilityImporter::new(root);
    // The operator's own home offers skills as well as the workspace does:
    // `~/.claude/skills` and `~/.codex/skills`, the same directories the
    // ecosystems themselves read. A switch that is off, or a skill switched
    // off by name, contributes nothing — one rule for every source of skills.
    let user_skills = {
        let homes = compat_homes();
        arsy_compat::skills::all(
            &homes,
            config.compat_enabled("claude"),
            config.compat_enabled("codex"),
        )
    };
    // The workspace is listed first and claims its names, so a repository that
    // ships a skill by the same name as one of the operator's own is the one
    // that skill means: project overrides home, the precedence the ecosystems
    // use.
    let mut listed: Vec<Skill> = Vec::new();
    let mut taken = std::collections::HashSet::new();
    for ecosystem in [Ecosystem::Claude, Ecosystem::Codex, Ecosystem::Omp] {
        workspace_skills(&mut listed, &mut taken, ecosystem, config, root, &importer);
    }
    listed.extend(
        user_skills
            .into_iter()
            .filter(|skill| {
                !config
                    .skill_disabled()
                    .contains(&format!("{}/{}", skill.ecosystem, skill.name))
                    && !taken.contains(&skill.name)
            })
            .map(|skill| Skill {
                name: skill.name,
                ecosystem: skill.ecosystem.to_owned(),
                description: skill_description(root, &skill.path.display().to_string()),
                path: skill.path.display().to_string(),
            }),
    );
    listed
}

/// The workspace skills of one ecosystem, appended to `listed` under their
/// names.
///
/// `taken` collects the names as they are listed, and the workspace is listed
/// before the operator's home: a name claimed here is the one the home copy
/// then yields to.
fn workspace_skills(
    listed: &mut Vec<arsy_code::agent::instructions::Skill>,
    taken: &mut std::collections::HashSet<String>,
    ecosystem: arsy_code::compat::Ecosystem,
    config: &arsy_kernel::config::Config,
    root: &Path,
    importer: &arsy_code::compat::CompatibilityImporter,
) {
    // A source the operator switched off contributes nothing, the same as its
    // hooks and its instructions: `compat.<source>.enabled` governs everything
    // that source's files carry.
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
        let key = format!("{}/{}", ecosystem.as_str(), name);
        if config.skill_disabled().contains(&key) || taken.contains(&name) {
            continue;
        }
        let Some(path) = skill["source"].as_str().map(str::to_owned) else {
            continue;
        };
        taken.insert(name.clone());
        listed.push(arsy_code::agent::instructions::Skill {
            name,
            ecosystem: ecosystem.as_str().to_owned(),
            description: skill_description(root, &path),
            path,
        });
    }
}

/// The one line a `SKILL.md` front matter offers as its description.
///
/// `fs.read` can carry the whole file to the model, so a body that fails to
/// read costs the skill its description and nothing more: the name and the
/// path are still enough for the model to find it when it wants to.
fn skill_description(root: &Path, relative: &str) -> Option<String> {
    let path = root.join(relative.trim_start_matches("./"));
    let text = std::fs::read_to_string(path).ok()?;
    let body = text.strip_prefix("---")?;
    let front = body.split("---").next()?;
    front.lines().find_map(|line| {
        let value = line.strip_prefix("description:")?;
        let description = value.trim().trim_matches('"').trim_matches('\'');
        (!description.is_empty()).then(|| description.to_owned())
    })
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
    use crate::picker::session::reconstruct_session_conversation;
    use crate::picker::wizard::effort_line;
    use crate::picker::wizard::{auth_step, provider_step, AuthNext, ProviderNext};
    use crate::run::{charge, is_stale_oauth_token, read_image, MAX_IMAGE_BYTES};
    #[cfg(all(feature = "tui", unix))]
    use crate::turn::drive_provider;
    use crate::turn::{block_gap, native_turn, stream_row, Painter, Streaming};
    use arsy_kernel::provider::Effort;
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

        // What is there already is never replaced. Staged and renamed rather
        // than written in place: `ARSY_CONFIG_HOME` is process-global, so a
        // test running beside this one resolves configuration from this very
        // file, and a truncating write is visible while it is still empty —
        // the hazard `bootstrap_user_config` stages against for the same
        // reason.
        let staged = home.join("arsy.json.test-tmp");
        std::fs::write(&staged, "{\"model\": {\"default\": \"m1\"}}\n").unwrap();
        std::fs::rename(&staged, &path).unwrap();
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

    /// A credential the catalog names but nothing can open — here a handle
    /// left behind by a build that still read the platform keyring — must not
    /// abort a turn that never asks for it.
    #[test]
    fn a_credential_that_will_not_open_does_not_fail_the_turn() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|held| held.into_inner());
        // Unique per run, not per process: every test in this binary shares
        // the process id, and one that writes into the configuration home
        // while this one is tearing it down would fail the teardown rather
        // than the assertion it came for.
        let home = std::env::temp_dir().join(format!(
            "arsy-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
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

        redactor(&mut Emitter::new(Output::Ci))
            .expect("a handle that will not open leaves the turn alone");

        assert_eq!(
            arsy_kernel::secret::WithdrawnOsStore.resolve("arsy-no-such-credential"),
            Err(arsy_kernel::secret::SecretError::WithdrawnStore(
                SecretHandle::new(OS_STORE_ID, "arsy-no-such-credential").unwrap()
            )),
            "the withdrawn store answers for its own handles rather than reading as a typo"
        );

        std::env::remove_var(arsy_kernel::config::CONFIG_HOME_VAR);
        // Best-effort: the subject of this test is the resolution above, not
        // whether the temporary directory could be swept up afterwards.
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_stale_oauth_token_is_the_only_failure_worth_a_fresh_credential() {
        let auth = ProviderError::Auth("expired".to_owned());
        assert!(is_stale_oauth_token(
            &auth,
            provider::CredentialSource::OAuth
        ));
        for other in [
            provider::CredentialSource::ConfiguredEnv,
            provider::CredentialSource::File,
            provider::CredentialSource::DefaultEnv,
            provider::CredentialSource::None,
        ] {
            assert!(
                !is_stale_oauth_token(&auth, other),
                "an API key rejected once will be rejected identically again: {other:?}"
            );
        }
        assert!(!is_stale_oauth_token(
            &ProviderError::RateLimited { retry_after: None },
            provider::CredentialSource::OAuth
        ));
    }

    /// A provider that replays a scripted round per request and records what
    /// it was asked, so a test can assert on the conversation the loop built.
    #[cfg(feature = "tui")]
    struct Scripted {
        descriptor: arsy_kernel::provider::ProviderDescriptor,
        rounds: std::sync::Mutex<std::collections::VecDeque<Vec<ModelEvent>>>,
        seen: std::sync::Mutex<Vec<CanonicalModelRequest>>,
        /// Errors to fail `stream` with before falling through to `rounds`,
        /// oldest first. Empty for every existing test, which never fails.
        fail_first:
            std::sync::Mutex<std::collections::VecDeque<arsy_kernel::provider::ProviderError>>,
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
            if let Some(error) = self.fail_first.lock().unwrap().pop_front() {
                return Err(error);
            }
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
            fail_first: std::sync::Mutex::new(std::collections::VecDeque::new()),
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
            context_window: None,
        };
        (resolved, scripted)
    }

    /// Like [`resolved`], but the provider fails its first `stream` call
    /// with `error` before falling through to the scripted rounds, and its
    /// credential is sourced from OAuth rather than an environment
    /// variable — for a test that exercises recovery from a stale token
    /// mid-session.
    #[cfg(feature = "tui")]
    fn resolved_failing_first(
        error: arsy_kernel::provider::ProviderError,
        rounds: Vec<Vec<ModelEvent>>,
    ) -> (provider::Resolved, std::sync::Arc<Scripted>) {
        let (mut resolved, scripted) = resolved(rounds);
        scripted.fail_first.lock().unwrap().push_back(error);
        resolved.source = provider::CredentialSource::OAuth;
        (resolved, scripted)
    }

    /// A window probelm reported can only shrink the transcript budget.
    #[cfg(feature = "tui")]
    #[test]
    fn a_reported_window_shrinks_the_budget_but_never_raises_it() {
        let (mut resolved, _) = resolved(Vec::new());
        resolved.endpoint.max_output_tokens = 8_192;
        let budget = |window: Option<u64>, resolved: &mut provider::Resolved| {
            resolved.context_window = window;
            run::context_budget(resolved)
        };
        assert_eq!(budget(Some(32_000), &mut resolved), 23_808);
        assert_eq!(budget(None, &mut resolved), 96_000 - 8_192);
        assert_eq!(budget(Some(1_000_000), &mut resolved), 96_000 - 8_192);
        assert_eq!(budget(Some(u64::MAX), &mut resolved), 96_000 - 8_192);
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
            None,
            &mut Emitter::new(Output::Json),
            &[],
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
        let (mut resolved, _) = resolved(vec![
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
                declaration: "test#PreToolUse[0].0".to_owned(),
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
            &mut resolved,
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
        let (mut resolved, scripted) = resolved(vec![
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
            &mut resolved,
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
        let (mut resolved, scripted) = resolved(vec![
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
        let approval = std::sync::Arc::new(approval::ApprovalCell::new(
            approval::ApprovalMode::BypassPermissions,
        ));
        let (_keys_sender, keys) = std::sync::mpsc::channel();
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "run the command once".to_owned(),
            }],
        }];
        let turn = native_turn(
            &mut resolved,
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

    /// A model that calls the same failing tool forever is stopped after
    /// three identical failures — with a message that names the loop, not
    /// the provider — instead of burning the whole round budget on it.
    #[cfg(feature = "tui")]
    #[test]
    fn a_model_repeating_one_failing_call_is_stopped_as_a_loop() {
        let workspace = tempfile::tempdir().unwrap();
        // The same denied call, round after round: policy refuses it in
        // `arsy run`, but here the operator denies it — the `d` answer.
        let denied = vec![ModelEvent::ToolCallCompleted {
            index: 0,
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            arguments: json!({"command": "touch looped"}),
        }];
        let mut rounds = Vec::new();
        for _ in 0..4 {
            let mut round = denied.clone();
            round.push(ModelEvent::Completed {
                stop: arsy_kernel::provider::StopReason::ToolUse,
            });
            rounds.push(round);
        }
        let (mut resolved, _scripted) = resolved(rounds);
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"ddd", std::sync::Arc::clone(&approval));
        let mut conversation = Vec::new();
        let turn = native_turn(
            &mut resolved,
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

        let failure = turn.failure.as_deref().unwrap_or_default();
        assert!(
            failure.contains("repeated the same failing tool call"),
            "{failure}"
        );
        assert!(
            !workspace.path().join("looped").exists(),
            "the denied call never ran"
        );
    }

    /// A model told the round budget is nearly gone hears it before the turn
    /// is cut: the last tool result carries the wrap-up note.
    #[cfg(feature = "tui")]
    #[test]
    fn the_wrap_up_warning_reaches_the_model_before_the_budget_ends() {
        let workspace = tempfile::tempdir().unwrap();
        // One tool round, then the answer. The default budget is far larger
        // than two rounds, so this only checks the plumbing, not the
        // threshold: the note appears when the *configured* budget is small.
        let (mut resolved, _scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "printf x"}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "done\n".to_owned(),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        let approval = std::sync::Arc::new(approval::ApprovalCell::new(
            approval::ApprovalMode::BypassPermissions,
        ));
        let (_keys_sender, keys) = std::sync::mpsc::channel();
        let mut conversation = Vec::new();
        let turn = native_turn(
            &mut resolved,
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
        assert_eq!(turn.response.trim(), "done");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_declined_tool_call_does_not_run_and_the_model_is_told_so() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut resolved, _scripted) = resolved(vec![
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
            &mut resolved,
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
        let (mut resolved, scripted) = resolved(vec![asking(), asking()]);
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (typist, keys, done) = typed(b"\x03", std::sync::Arc::clone(&approval));
        let mut conversation = Vec::new();
        // A stop is answered by the stop, not by waiting for the keyboard to
        // hang up: the calls after it are refused without asking.
        let turn = native_turn(
            &mut resolved,
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

    /// A token that expires mid-session is reported after a refresh is
    /// attempted, not silently swallowed. The endpoint id ("stub") names no
    /// real provider, so `provider::resolve` cannot actually refresh it —
    /// this exercises the failure path deterministically: refresh is
    /// attempted and, failing, the original failure still reaches the
    /// operator, and no second `stream` call is made on top of it.
    #[cfg(feature = "tui")]
    #[test]
    fn a_stale_oauth_token_mid_session_is_reported_after_a_refresh_attempt() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut resolved, scripted) = resolved_failing_first(
            ProviderError::Auth("token expired".to_owned()),
            vec![vec![ModelEvent::Completed {
                stop: arsy_kernel::provider::StopReason::EndTurn,
            }]],
        );
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        let (_keys_sender, keys) = std::sync::mpsc::channel();
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "hello".to_owned(),
            }],
        }];
        let turn = native_turn(
            &mut resolved,
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
        assert_eq!(
            turn.failure.as_deref(),
            Some("provider authentication failed: token expired")
        );
        assert!(matches!(turn.provider_error, Some(ProviderError::Auth(_))));
        assert_eq!(
            scripted.seen.lock().unwrap().len(),
            1,
            "a failed refresh must not be followed by a second stream call"
        );
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

        let providers = vec!["myai".to_owned()];
        let mut draft = tui::ProviderDraft::default();
        let step = |step: Step, line: &str, draft: &mut tui::ProviderDraft| {
            provider_step(step, line, draft, &providers)
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

    /// `ui.mcp_log` decides how much of a server's own logging survives to the
    /// transcript. A server that said something is never silently dropped at
    /// `summary`, and `hidden` means hidden.
    #[test]
    fn ui_mcp_log_keeps_every_server_visible_at_summary_and_none_at_hidden() {
        let logs = || {
            vec![
                ("mongodb".to_owned(), "[MCP Info] no tools file".to_owned()),
                ("mongodb".to_owned(), "npm warn deprecated".to_owned()),
                (
                    "postgres".to_owned(),
                    "Warning: deprecated argument".to_owned(),
                ),
            ]
        };

        assert!(mcp_log_rows(logs(), "hidden").is_empty());
        assert!(
            mcp_log_rows(Vec::new(), "full").is_empty(),
            "a quiet turn prints nothing whatever the level says"
        );

        let full = mcp_log_rows(logs(), "full");
        assert_eq!(full.len(), 3, "{full:?}");
        assert_eq!(full[0], "mcp mongodb: [MCP Info] no tools file");

        let summary = mcp_log_rows(logs(), "summary");
        assert_eq!(
            summary.len(),
            1,
            "one row for the turn, not one per server or per line: {summary:?}"
        );
        assert!(
            summary[0].contains("3 log lines"),
            "the row counts every line the servers wrote: {summary:?}"
        );
        assert!(
            summary[0].contains("2 servers"),
            "the row says how many servers spoke: {summary:?}"
        );
        assert!(
            summary[0].contains("ui.mcp_log"),
            "the summary says how to read the rest: {summary:?}"
        );

        // An unknown level is read as `summary` rather than as `full`: the
        // configuration refuses one at load, so this is only reachable by a
        // caller passing something odd, and the quiet reading is the safe one.
        assert_eq!(mcp_log_rows(logs(), "whatever").len(), 1);
    }

    /// The catalog lives beside the user configuration and nowhere else, and a
    /// file that still asks for the withdrawn keyring says so rather than being
    /// silently defaulted.
    #[test]
    fn the_credential_catalog_lives_in_a_file_and_the_keyring_is_refused_by_name() {
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

        // A name that is not a store is refused at load, not silently
        // defaulted: a typo must not quietly send credentials somewhere else.
        let error = write("schema_version = 1\n[credentials]\nstore = \"vault\"\n").unwrap_err();
        assert!(format!("{error}").contains("vault"), "{error}");

        // `os` is refused by name, with where it went, because an operator who
        // set it deliberately is owed more than "not one of: file".
        let error = write("schema_version = 1\n[credentials]\nstore = \"os\"\n").unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("keyring"), "{message}");
        assert!(message.contains("remove the key"), "{message}");

        // One store, and it is the one the `secret://` handles name, so one
        // vocabulary covers both the handle and the catalog.
        assert_eq!(CREDENTIAL_STORES, ["file"]);
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
                    | "/agents"
                    | "/skill"
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
    /// Blocks breathe and single rows do not.
    ///
    /// The first cut of the modern style wrote every block flush against the
    /// one before it, so a turn came out as one wall of borders and text with
    /// nothing to tell the steps apart.
    #[cfg(feature = "tui")]
    #[test]
    fn a_block_gets_one_blank_line_above_it_and_a_single_row_gets_none() {
        let card = "╭── $ cargo check ──╮\n│ ok │\n╰── ✓ done ──╯";
        let bullet = "  • fs.read AGENTS.md";

        tui::set_render_style(tui::RenderStyle::Modern);
        assert_eq!(block_gap(card), "\n", "a block is separated");
        assert_eq!(block_gap(bullet), "", "a single row stays tight");
        assert_eq!(modern_gap(), "\n");

        // Through the writer: the gap lands between the composer's erase
        // sequence and the card, so the card is reached across a blank line.
        // Asserted on the card rather than on a run of newlines, because the
        // composer's own chrome carries newlines of its own.
        let drawn = |row: &str| {
            let mut screen: Vec<u8> = Vec::new();
            let mut composer = tui::Composer::default();
            stream_row(&mut screen, &mut composer, false, "", "", row).unwrap();
            String::from_utf8(screen).expect("UTF-8 terminal output")
        };
        assert!(
            drawn(card).contains("\n╭── $ cargo check"),
            "a block is reached across a blank line"
        );
        assert!(
            !drawn(bullet).contains("\n  • fs.read"),
            "a single row is not"
        );

        // And through the other writer, which is the one the external Codex
        // route uses — the route this was reported from. Both writers have to
        // agree or the spacing depends on which provider is answering.
        let painted = |row: &str| {
            let mut screen: Vec<u8> = Vec::new();
            let mut composer = tui::Composer::default();
            let painter = Painter {
                colour: false,
                footer: "",
                width: std::cell::Cell::new(80),
                started: std::time::Instant::now(),
            };
            painter
                .row(&mut screen, &mut composer, Some(row), false, 0, 0)
                .unwrap();
            String::from_utf8(screen).expect("UTF-8 terminal output")
        };
        assert!(
            painted(card).contains("\n╭── $ cargo check"),
            "the Codex route separates a block too"
        );
        assert!(
            !painted(bullet).contains("\n  • fs.read"),
            "and leaves a single row tight"
        );

        // Classic keeps the spacing an operator who chose it already has.
        tui::set_render_style(tui::RenderStyle::Classic);
        assert_eq!(block_gap(card), "", "classic spacing is unchanged");
        assert_eq!(modern_gap(), "");
    }

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
        live.close(&mut screen, &mut composer, false, "", "status")
            .unwrap();

        let drawn = String::from_utf8(screen).unwrap();
        // Ordering rather than the closing glyph: the classic style ends the
        // reasoning block with `╰`, the modern one leaves it unboxed, and what
        // this is actually about is that the reasoning is finished with before
        // the answer starts — true of both.
        let reasoning = drawn
            .find("weighing it up")
            .expect("the reasoning is drawn");
        let header = drawn.find('✦').expect("the answer announces itself");
        let prose = drawn.find("the answer").expect("the answer is drawn");
        assert!(
            reasoning < header,
            "the reasoning is finished with before the header:\n{drawn}"
        );
        assert!(
            header < prose,
            "the header comes before the prose:\n{drawn}"
        );
    }

    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn a_follow_up_typed_during_a_provider_turn_is_carried_to_the_next_one() {
        let route = tui::ModelRoute {
            provider: "test-provider".to_owned(),
            model: "default".to_owned(),
        };
        let approval =
            std::sync::Arc::new(approval::ApprovalCell::new(approval::ApprovalMode::Default));
        // Queued on the keyboard before the turn starts rather than typed a
        // fixed number of milliseconds into it. The loop drains the keyboard on
        // every pass, so the follow-up is still read while the turn is running
        // — but which pass reads it no longer depends on how loaded the machine
        // is, which is what made this case fail inside the full suite and pass
        // on its own.
        let (sender, keys) = std::sync::mpsc::channel();
        for byte in b"next thing\r" {
            sender.send(*byte).expect("the keyboard is still open");
        }
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
        // Held open across the call: a closed keyboard reads as the operator
        // hanging up rather than as a turn with a follow-up waiting behind it.
        drop(sender);

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
            provider: "test-provider".to_owned(),
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
            provider: "test-provider".to_owned(),
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

#[cfg(test)]
mod theme_registry_tests {
    /// `/settings` offers `theme.base` from the kernel's registry, while the
    /// palettes themselves are arsy-tui's. Neither can depend on the other, so
    /// this test is what holds the two lists together: a theme added to one
    /// and not the other is either offered and unrenderable, or renderable and
    /// unreachable from the editor.
    #[test]
    fn the_settings_registry_names_exactly_the_built_in_themes() {
        let registry: Vec<&str> = arsy_kernel::config::THEME_BASES.to_vec();
        let rendered: Vec<&str> = crate::tui::THEMES.iter().map(|(name, _)| *name).collect();
        assert_eq!(registry, rendered, "the two theme lists have drifted");
        assert_eq!(
            arsy_kernel::config::DEFAULT_THEME_BASE,
            crate::tui::DEFAULT_THEME,
            "the two defaults are different themes"
        );
    }
}
