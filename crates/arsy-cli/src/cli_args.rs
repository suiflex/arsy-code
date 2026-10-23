//! Parse the CLI command vocabulary without opening a workspace.

use crate::{
    code, evidence, extensions, guard, integrations, mcp, memory, policy, provider, review,
    secret_failed, serve, session, storage, verify, Diagnostic, Output,
};
use arsy_kernel::{domain::SessionId, secret::SecretHandle};
use std::path::PathBuf;

/// Documented commands that a later phase ships, so they report their phase
/// instead of failing as unknown input.
const UNAVAILABLE: &[(&str, u8)] = &[("completions", 1)];

pub(super) const USAGE: &str = "\
arsy — agentic coding harness

Usage:
  arsy run <TASK> [--image <PATH>]   execute one task non-interactively
                             ('-' reads the task from stdin)
  arsy resume <SESSION_ID>   resume a recorded session
  arsy doctor                report platform, sandbox, credential, and config state
  arsy eval <SUITE> [--strict]  run an evaluation fixture; --strict needs its revision
  arsy compat explain <KIND> explain claude, codex, omp, or agents imports
  arsy config explain [KEY]  show effective configuration and where it came from
  arsy config set <KEY> <VALUE> [--scope <user|workspace>]  write one setting
  arsy config validate [PATH] [--strict]  check arsy.json without starting a session
  arsy config unset <KEY> [--scope <user|workspace>]  remove one setting
  arsy storage               list where ARSY keeps files and how big each is
  arsy storage clean <cache|repo-map|views|artifacts>  remove what ARSY rebuilds
  arsy storage reset-history --confirm <WORKSPACE NAME>  delete this workspace's sessions
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
  arsy mcp add <NAME> --command <CMD> [--env K=${V}]... [-- ARGS...]  define a stdio connection
  arsy mcp add <NAME> --transport http --url <URL> [--header K=${V}]...  define an HTTP connection
  arsy mcp remove|enable|disable <NAME> [--scope <user|workspace>]
  arsy mcp test <NAME> [--timeout <SECONDS>]  connect, negotiate, disconnect
  arsy hook list [--event <NAME>]      list lifecycle hooks and what runs
  arsy hook add --event <EVENT> [--matcher <PATTERN>] --command <CMD> [--timeout <SECONDS>] [--scope <user|workspace>]
  arsy hook remove <EVENT> <POSITION> [--scope <user|workspace>]  remove one from ARSY's guard.json
  arsy auth set <PROVIDER>   store a credential in the OS credential store
  arsy auth login <PROVIDER> sign in to a provider through its OAuth client
  arsy auth list             list credential handles (never values)
  arsy auth remove <HANDLE>  remove a credential from the OS credential store
  arsy update [--check] [--force] check or download the latest release for this OS

Global flags:
  --workspace <PATH>   workspace root (default: current directory)
  --config <PATH>      one extra config file, applied last; it cannot widen policy
  --add-dir <PATH>     another directory to work in beside the workspace, under
                       the same approval mode; repeat for more than one
  --provider <ID>      the endpoint this run dispatches to
  --model <ID>         the model this run asks for, within model.allowed
  --output <MODE>      human, json, or ci
  --no-color           disable ANSI styling
  --debug              trace the agent loop to stderr as JSON lines:
                       requests, normalized model events, tool results, retries
  --help, --version
";

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
    /// `arsy config validate [PATH] [--strict]`: whether every layer, or the
    /// one file named, loads, and what the loader dropped from it.
    ConfigValidate {
        path: Option<PathBuf>,
        strict: bool,
    },
    /// `arsy hook add|remove`: edit ARSY's own `guard.json`.
    Hook {
        request: guard::Request,
    },
    /// `arsy storage [clean <TARGET> | reset-history --confirm <NAME>]`.
    Storage {
        request: storage::Request,
    },
    /// `arsy config set <KEY> <VALUE> [--scope user|workspace]`.
    ConfigSet {
        key: String,
        value: String,
        scope: mcp::Scope,
    },
    /// `arsy config unset <KEY> [--scope user|workspace]`.
    ConfigUnset {
        key: String,
        scope: mcp::Scope,
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
        /// `--retention`, or `None` for `storage.artifact_retention_days`.
        retention_ms: Option<u64>,
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
        /// `--env` or `--header` as written: `${NAME}` references, never the
        /// values, which `server` would hold once expanded.
        references: std::collections::BTreeMap<String, String>,
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
    /// `--dangerously-skip-permissions`: start the interactive session in
    /// `bypassPermissions`, once the operator confirms the warning.
    pub skip_permissions: bool,
    /// `--config <PATH>`: one extra configuration file, applied after every
    /// discovered layer. It cannot weaken policy — ceilings intersect.
    pub config: Option<PathBuf>,
    /// `--add-dir <PATH>`, repeatable: directories to work in beside the
    /// workspace for this invocation, on top of
    /// `execution.additional_directories`.
    pub additional_directories: Vec<PathBuf>,
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
        skip_permissions: parsed.skip_permissions,
        config: parsed.config.take(),
        additional_directories: std::mem::take(&mut parsed.additional_directories),
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
    ("storage", storage::parse),
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
        "config" => parse_config(parsed.positional, parsed.scope.as_deref(), parsed.strict)?,
        "hook" => match guard::parse(&parsed) {
            Some(command) => command?,
            None => integrations::parse("hook", parsed.positional, parsed.source, parsed.event)?,
        },
        other => return Err(unknown_command(other)),
    })
}

#[derive(Default)]
pub(super) struct ParsedArguments {
    pub(super) workspace: Option<PathBuf>,
    pub(super) output: Option<Output>,
    pub(super) no_color: bool,
    pub(super) check: bool,
    pub(super) follow: bool,
    pub(super) strict: bool,
    pub(super) force: bool,
    pub(super) turns: bool,
    pub(super) evidence: bool,
    pub(super) include_artifacts: bool,
    pub(super) apply: bool,
    pub(super) all: bool,
    pub(super) capabilities: bool,
    pub(super) dry_run: bool,
    /// `--debug`: trace the agent loop to stderr as JSON lines.
    pub(super) debug: bool,
    /// `--dangerously-skip-permissions`: start the TUI in bypass mode.
    pub(super) skip_permissions: bool,
    pub(super) handle: Option<String>,
    pub(super) trials: Option<u32>,
    pub(super) limit: Option<usize>,
    pub(super) max_bytes: Option<u64>,
    pub(super) out: Option<PathBuf>,
    pub(super) source: Option<String>,
    pub(super) event: Option<String>,
    pub(super) to: Option<String>,
    pub(super) at: Option<String>,
    pub(super) retention: Option<String>,
    /// `arsy storage reset-history --confirm`: the workspace name, typed.
    pub(super) confirm: Option<String>,
    /// `arsy code symbol --tier`: which tier answers.
    pub(super) tier: Option<String>,
    /// `arsy migrate --backup`: where the pre-migration copy goes.
    pub(super) backup: Option<PathBuf>,
    /// `arsy review --base`: the revision the working tree is compared against.
    pub(super) base: Option<String>,
    pub(super) resource: Option<String>,
    pub(super) actor: Option<String>,
    /// `--config <PATH>`: one extra configuration file for this invocation.
    pub(super) config: Option<PathBuf>,
    /// `--add-dir <PATH>`, repeatable: directories beside the workspace.
    pub(super) additional_directories: Vec<PathBuf>,
    /// `--image <PATH>`: a picture attached to the prompt.
    pub(super) image: Option<PathBuf>,
    /// `--provider <ID>`: a global override, and the filter `model list` reads.
    pub(super) provider: Option<String>,
    /// `--model <ID>`: a global override of the model a turn asks for.
    pub(super) model: Option<String>,
    pub(super) capability: Option<String>,
    pub(super) transport: Option<String>,
    pub(super) protocol: Option<String>,
    pub(super) command: Option<String>,
    /// `arsy hook add --matcher`: which subjects a hook applies to.
    pub(super) matcher: Option<String>,
    pub(super) url: Option<String>,
    /// `arsy mcp add --env KEY=VALUE`, repeatable: a stdio server's variables.
    pub(super) env: Vec<String>,
    /// `arsy mcp add --header KEY=VALUE`, repeatable: an http server's headers.
    pub(super) headers: Vec<String>,
    pub(super) scope: Option<String>,
    pub(super) timeout: Option<u64>,
    pub(super) name: Option<String>,
    pub(super) positional: Vec<String>,
    pub(super) early: Option<Command>,
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
        if apply_value_flag(&argument, &mut parsed, &mut arguments)?
            || apply_management_flag(&argument, &mut parsed, &mut arguments)?
            || apply_directory_flag(&argument, &mut parsed, &mut arguments)?
            || apply_launch_flag(&argument, &mut parsed, &mut arguments)?
        {
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

/// The switches every command accepts, apart from the per-command table below
/// so each stays a list short enough to read.
fn apply_global_switch(argument: &str, parsed: &mut ParsedArguments) -> bool {
    match argument {
        "--help" | "-h" => parsed.early = Some(Command::Help),
        "--version" | "-V" => parsed.early = Some(Command::Version),
        "--no-color" => parsed.no_color = true,
        "--debug" => parsed.debug = true,
        "--dangerously-skip-permissions" => parsed.skip_permissions = true,
        _ => return false,
    }
    true
}

fn apply_switch(argument: &str, parsed: &mut ParsedArguments) -> bool {
    if apply_global_switch(argument, parsed) {
        return true;
    }
    match argument {
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
        _ => return false,
    }
    true
}

/// The flags `arsy storage` and `arsy hook add` take, apart from the shared
/// table above so each stays a list short enough to read.
fn apply_management_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    let slot = match argument {
        "--confirm" => &mut parsed.confirm,
        "--matcher" => &mut parsed.matcher,
        _ => return Ok(false),
    };
    *slot = Some(value(arguments, argument)?);
    Ok(true)
}

/// `--add-dir <PATH>`: repeatable, so it collects rather than fills a slot.
fn apply_directory_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    if argument != "--add-dir" {
        return Ok(false);
    }
    parsed
        .additional_directories
        .push(PathBuf::from(value(arguments, argument)?));
    Ok(true)
}

/// `--env` and `--header` for `arsy mcp add`: repeatable, so they collect.
fn apply_launch_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    let collected = match argument {
        "--env" => &mut parsed.env,
        "--header" => &mut parsed.headers,
        _ => return Ok(false),
    };
    collected.push(value(arguments, argument)?);
    Ok(true)
}

fn apply_value_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    if argument == "--output" {
        parsed.output = Some(output_mode(&value(arguments, argument)?)?);
        return Ok(true);
    }
    if apply_path_value(argument, parsed, arguments)? {
        return Ok(true);
    }
    if apply_numeric_value(argument, parsed, arguments)? {
        return Ok(true);
    }
    if apply_endpoint_value(argument, parsed, arguments)? {
        return Ok(true);
    }
    apply_context_value(argument, parsed, arguments)
}

fn apply_path_value(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    let slot = match argument {
        "--workspace" => &mut parsed.workspace,
        "--out" => &mut parsed.out,
        "--backup" => &mut parsed.backup,
        "--config" => &mut parsed.config,
        "--image" => &mut parsed.image,
        _ => return Ok(false),
    };
    *slot = Some(PathBuf::from(value(arguments, argument)?));
    Ok(true)
}

fn apply_numeric_value(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    match argument {
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

fn apply_endpoint_value(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    let slot = match argument {
        "--provider" => &mut parsed.provider,
        "--model" => &mut parsed.model,
        "--capability" => &mut parsed.capability,
        "--transport" => &mut parsed.transport,
        "--protocol" => &mut parsed.protocol,
        "--command" => &mut parsed.command,
        "--url" => &mut parsed.url,
        _ => return Ok(false),
    };
    *slot = Some(value(arguments, argument)?);
    Ok(true)
}

fn apply_context_value(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    let slot = match argument {
        "--handle" => &mut parsed.handle,
        "--source" => &mut parsed.source,
        "--event" => &mut parsed.event,
        "--to" => &mut parsed.to,
        "--at" => &mut parsed.at,
        "--retention" => &mut parsed.retention,
        "--tier" => &mut parsed.tier,
        "--base" => &mut parsed.base,
        "--resource" => &mut parsed.resource,
        "--actor" => &mut parsed.actor,
        "--scope" => &mut parsed.scope,
        _ => return Ok(false),
    };
    *slot = Some(value(arguments, argument)?);
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
    skip_permissions: bool,
    config: Option<PathBuf>,
    additional_directories: Vec<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
}

fn invocation(global: Global, command: Command) -> Invocation {
    Invocation {
        workspace: global.workspace.unwrap_or_else(|| PathBuf::from(".")),
        output: global.output,
        no_color: global.no_color,
        debug: global.debug,
        skip_permissions: global.skip_permissions,
        config: global.config,
        additional_directories: global.additional_directories,
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
pub(super) const INSPECTIONS: &[(&str, &[&str], Option<&str>)] = &[
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
pub(super) fn inspection_args(line: &str) -> Option<Vec<String>> {
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

/// `config explain [KEY]`, `validate [PATH]`, `set`, and `unset`.
fn parse_config(
    positional: Vec<String>,
    scope: Option<&str>,
    strict: bool,
) -> Result<Command, Diagnostic> {
    match positional.first().map(String::as_str) {
        Some("validate") if positional.len() <= 2 => Ok(Command::ConfigValidate {
            path: positional.get(1).map(PathBuf::from),
            strict,
        }),
        Some("validate") => Err(usage("config validate accepts only [PATH]")),
        Some("explain") if positional.len() <= 2 => Ok(Command::ConfigExplain {
            key: positional.into_iter().nth(1),
        }),
        Some("explain") => Err(usage("config explain accepts only [KEY]")),
        Some("set") if positional.len() == 3 => Ok(Command::ConfigSet {
            key: positional[1].clone(),
            value: positional[2].clone(),
            scope: mcp::Scope::parse(scope)?,
        }),
        Some("set") => Err(usage("config set requires <KEY> <VALUE>")),
        Some("unset") if positional.len() == 2 => Ok(Command::ConfigUnset {
            key: positional[1].clone(),
            scope: mcp::Scope::parse(scope)?,
        }),
        Some("unset") => Err(usage("config unset requires <KEY>")),
        _ => Err(usage("config requires explain, validate, set, or unset")),
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

pub(super) fn only_argument(
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

pub(super) fn usage(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(
        "ARSY-SCH-1001",
        message,
        "run `arsy --help` for the surface",
    )
}
