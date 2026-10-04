<p align="center"><img src="assets/logo.png" alt="ARSY logo" width="220" /></p>
<h1 align="center">ARSY CODE</h1>
<p align="center"><strong>A local, auditable, model-independent software-engineering agent harness.</strong></p>
<p align="center">Plan · Orchestrate · Execute</p>

<p align="center"><img src="assets/product-flow.png" alt="ARSY CODE flow from task planning through hooks, policy, tools, evidence, and verification" width="100%" /></p>

**ARSY** is the core platform; this repository contains **ARSY CODE**, its terminal interface.

> [!NOTE]
> ARSY CODE is under active development. The CLI ships as a signed release for six
> Tier 1 targets and installs from npm, Homebrew, Scoop, or the release archives,
> while some commands and integrations remain roadmap-gated.

## Why ARSY CODE?

A capable coding agent needs more than a model connection and shell access. It must understand repository instructions, preserve long-running context, select the right capability, request approval at the correct boundary, coordinate parallel work, and leave an auditable trail.

- **Provider-neutral** — select models per task without changing the runtime.
- **Policy-first** — every effect passes through permission evaluation, sandboxing, and audit.
- **Durable** — sessions, events, operation results, and checkpoints survive restarts.
- **Composable** — support operations, MCP, skills, hooks, and `AGENTS.md`.
- **Multi-agent** — represent work as a dependency graph with explicit budgets and concurrency limits.
- **Observable** — keep tokens, cost, latency, edits, approvals, and verification traceable.

## Intended experience

```console
$ arsy
ARSY CODE · workspace: ~/code/payments · model: auto

› find the cause of the checkout timeout, make the smallest fix, then run the relevant tests

  ✓ read AGENTS.md and Git status
  ✓ mapped the checkout request flow
  ! approval required: run integration tests with local network access
  → approve once / approve rule / deny
```

```console
arsy                                      # interactive TUI
arsy run "fix bug #42"                    # non-interactive execution
arsy resume <session-id>                   # resume a session
arsy session list                          # find a session to resume
arsy session show <session-id> --turns     # inspect recorded turns
arsy review                                # review local changes
arsy auth set anthropic                    # store a provider credential as a handle
arsy provider list                         # list allowed providers
arsy model list                            # list allowed models
arsy config explain policy                 # show effective config and its source
arsy policy explain fs.write               # ask whether an operation would be allowed
arsy code symbol Workspace                 # find a declaration
arsy mcp add docs --transport stdio --command ...  # define an MCP connection
arsy serve --protocol mcp                   # expose operations over stdio
arsy doctor                                # diagnose the environment
```

The full surface — session, configuration, policy, credentials, connections, extensions, evidence,
and maintenance commands — is specified in [CLI and TUI surface](docs/36-cli-tui.md). Run
`arsy --help` for the command surface available in the current build.

## Install with npm

```console
npm install --global @suiflex/arsy-code
arsy --version
```

Installing downloads the binary for the host platform from the matching GitHub
Release and verifies its SHA-256, so the install needs network access. macOS,
glibc Linux, and Windows are supported on x86-64 and ARM64. Run `arsy doctor`
afterwards to inspect the active build and environment.

## Install with Homebrew

```console
brew install suiflex/tap/arsy-code
```

## Install with Scoop

```console
scoop bucket add suiflex https://github.com/suiflex/scoop-bucket
scoop install arsy-code
```

## Install with curl / PowerShell

```console
curl -fsSL https://github.com/suiflex/arsy-code/releases/latest/download/install.sh | sh
```

```powershell
irm https://github.com/suiflex/arsy-code/releases/latest/download/install.ps1 | iex
```

Both scripts verify the downloaded archive's SHA-256 checksum before
installing, but not the Sigstore signature. Every release publishes a
`SHA256SUMS` signed through keyless Sigstore, verifiable with the bundle beside
it — see [Install and verify](docs/34-distribution.md#install-and-verify) when
that stronger guarantee matters.

### Complete command reference

```console
arsy
arsy run <TASK> [--workspace <PATH>] [--output human|json|ci]
arsy resume <SESSION_ID> [--follow]
arsy review [REVISION] [--base <REVISION>] [--strict]
arsy doctor [--strict]
arsy update [--check] [--force]            # check for updates or install the latest release

arsy session list [--workspace-only] [--limit <N>]
arsy session show <SESSION_ID> [--turns] [--evidence]
arsy session export <SESSION_ID> [--out <PATH>] [--include-artifacts]
arsy session rewind <SESSION_ID> --to <EVENT_ID>
arsy session fork <SESSION_ID> [--at <EVENT_ID>]

arsy config explain [KEY]
arsy config validate [PATH] [--strict]
arsy config set <KEY> <VALUE> [--scope user|workspace]
arsy config unset <KEY> [--scope user|workspace]
arsy compat explain <claude|codex|omp> [--loss-only]
arsy policy explain <OPERATION> [--resource <REF>] [--actor <ID>]
arsy code symbol <NAME> [--tier auto|text] [--limit <N>]
arsy code explain <SYMBOL_ID>
arsy code references <SYMBOL_ID>
arsy code diagnostics <PATH>

arsy auth set <PROVIDER> [--handle <NAME>]
arsy auth login <PROVIDER>
arsy auth list
arsy auth remove <HANDLE> [--force]
arsy provider list [--all]
arsy model list [--provider <ID>] [--capability <NAME>]

arsy mcp list [--source <KIND>]
arsy mcp show <NAME> [--source <KIND>]
arsy mcp add <NAME> --command <CMD> [-- ARGS...]
arsy mcp add <NAME> --transport http --url <URL>
arsy mcp remove <NAME> [--scope user|workspace]
arsy mcp enable <NAME> [--scope user|workspace]
arsy mcp disable <NAME> [--scope user|workspace]
arsy mcp test <NAME> [--timeout <SECONDS>]
arsy mcp reconnect <NAME> [--all] [--timeout <SECONDS>]
arsy mcp refresh <NAME> [--all]

arsy skill list [--source <ECOSYSTEM>]
arsy hook list [--event <NAME>]
arsy hook add --event <EVENT> [--matcher <PATTERN>] --command <CMD> [--timeout <SECONDS>] [--scope user|workspace]
arsy hook remove <EVENT> <POSITION> [--scope user|workspace]
arsy plugin list [--capabilities]
arsy plugin install <SOURCE> [--scope user|workspace] [--force]
arsy plugin inspect <ID>
arsy plugin remove <ID> [--force]
arsy plugin run <ID> [--to <INPUT>]
arsy plugin refresh [ID] [--dry-run]

arsy artifact show <REF> [--max-bytes <N>]
arsy artifact export <REF> --out <PATH>
arsy memory list [--scope <SCOPE>] [--all]
arsy memory remember <CLAIM> [--scope <SCOPE>]
arsy memory forget <ID> [--to <REASON>]
arsy gc [--apply] [--retention <DURATION>]
arsy storage
arsy storage clean <cache|repo-map|views|artifacts>
arsy storage reset-history --confirm <WORKSPACE NAME>
arsy migrate [--apply] [--backup <PATH>]
arsy eval <SUITE> [--trials <N>] [--strict] [--out <PATH>]
arsy serve [--protocol mcp|acp] [--transport stdio]
arsy completions <bash|zsh|fish|powershell>  # roadmap-gated: reports a diagnostic and exits 1
```

Global options can be used with commands that support them:
`--workspace <PATH>`, `--config <PATH>`, `--provider <ID>`, `--model <ID>`,
`--output human|json|ci`, `--no-color`, `--debug`, `--help`, and `--version`.
Run `arsy --help` for the exact syntax and availability of the current build. Commands that are
roadmap-gated report a diagnostic instead of silently behaving differently.

## Where ARSY keeps files

ARSY keeps the operator's own files in `~/.arsy/` (or wherever
`ARSY_CONFIG_HOME` points) and a project's in `<workspace>/.arsy/`. In both,
the files a person edits sit at the top, and everything ARSY writes on its
own goes one level down:

```
~/.arsy/                       <workspace>/.arsy/
├── arsy.json                  ├── arsy.json
├── guard.json                 ├── guard.json
├── secrets/        (0700)     ├── AGENTS.md
│   ├── credentials.json       ├── plugins/
│   └── <handle>    (0600)     └── state/              (ignores itself)
├── cache/                         ├── sessions.sqlite3
│   └── mcp-tools.json             ├── repo-map.json
└── state/                         ├── artifacts/
    └── model, effort, theme       ├── views/
                                   └── eval/
```

- **Top level** — settings and hooks. A project's `.arsy/arsy.json`,
  `guard.json`, `AGENTS.md`, and `plugins/` are meant to be committed.
- **`state/`, `cache/`** — safe to delete; ARSY rebuilds what it needs.
  Deleting a workspace's `state/` loses its session history and nothing else.
  `.arsy/state/` writes its own `.gitignore`, so a repository needs no entry
  for it (`storage.state_gitignore` switches that off).
- **`secrets/`** — owner-only, written only through `/provider` and
  `arsy auth`.

A layout from an earlier release is moved on first use, by rename and never
over an existing file. Old subagent views are git worktrees and stay put until
`arsy storage clean views` removes them.

Configuration resolves enterprise → `~/.arsy/arsy.json` →
`<workspace>/.arsy/arsy.json` → nested `.arsy/arsy.json` files toward the
working directory; later layers win within the limits
[docs/35](docs/35-configuration.md) sets. That document lists every key, its
type, default, and which layers may set it, with a complete example. Add
`"$schema": "https://raw.githubusercontent.com/suiflex/arsy-code/main/schemas/arsy.schema.json"`
to the file for completion and inline errors in an editor. Everything above can
be managed without editing a file:

| Task | In the TUI | From a shell |
|---|---|---|
| Change a setting for you or for the project | `/settings`, Tab switches user/project | `arsy config set <KEY> <VALUE> [--scope workspace]` |
| See where a value came from | `/settings [KEY]` | `arsy config explain [KEY]` |
| Check a hand-edited `arsy.json` | warnings print when the session starts | `arsy config validate [PATH] [--strict]` |
| See what ARSY stores and how big it is | `/storage` | `arsy storage` |
| Clear caches, idle views, old artifacts | `/storage` | `arsy storage clean <target>` |
| Delete this workspace's session history | `/storage`, then type the workspace name | `arsy storage reset-history --confirm <name>` |
| Add or remove a hook | `/hooks`, then `a` or `x` | `arsy hook add ...` / `arsy hook remove ...` |

## Design documents

- [Complete architecture specification](docs/INDEX.md)
- [Product requirements](docs/02-product-requirements.md)
- [System architecture](docs/04-system-architecture.md)
- [Rust workspace and technology choices](docs/05-rust-workspace.md)
- [Competitive research](docs/01-competitive-research.md) and [claim ledger](docs/report-source.md)
- [Accepted ADRs](docs/ADR/)

## Current implementation baseline

1. Interactive TUI and non-interactive execution.
2. Official Anthropic and OpenAI API adapters.
3. File, search, patch, shell, and read-only Git operations.
4. Hierarchical instructions through `AGENTS.md`.
5. Approval policy, workspace sandbox, and audit log.
6. Persistent sessions, resume, context compaction, and token/cost summaries.
7. MCP client support for stdio and Streamable HTTP.

These paths exist at different maturity levels; the source-audited status is in
the [engineering roadmap](docs/31-roadmap.md). Durable asynchronous agents and
isolated writer integration are the next P0 runtime work. Remote runners,
plugin marketplaces, and a default daemon remain deferred.

## Security principles

- Model and operation output is always untrusted.
- Read and write are separate capabilities.
- Network, filesystem, process, and credentials have separate policies.
- Destructive commands cannot rely on generic approval.
- Secrets never enter prompts or event logs.
- Every external effect has an operation ID, status, and result evidence.

See the [threat model](docs/29-threat-model.md).

## Status

| Area | Status |
|---|---|
| Product and architecture specification | Complete |
| CLI implementation | Active development |
| Distribution | Released — npm, Homebrew, Scoop, and signed release archives |
| Stable API | Not available |

## License

ARSY CODE is an independent open-source project licensed under the [MIT License](LICENSE). It is not affiliated with Anthropic or OpenAI.
