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

## Install Kurir

[Kurir](https://github.com/suiflex/kurir) is the SuiFlex companion toolkit for
registering MCP servers with supported agent harnesses. Install it alongside
ARSY CODE when you want one portable MCP registration workflow:

```console
npm install --global @suiflex/kurir
kurir clients
```

Other installation channels:

```console
cargo install kurir
curl -fsSL https://raw.githubusercontent.com/suiflex/kurir/main/scripts/install.sh | sh
brew install suiflex/tap/kurir
```

```powershell
irm https://raw.githubusercontent.com/suiflex/kurir/main/scripts/install.ps1 | iex
```

On Windows with Scoop:

```console
scoop bucket add suiflex https://github.com/suiflex/scoop-bucket
scoop install kurir
```

See the [Kurir README](https://github.com/suiflex/kurir#install) for platform
details and registration examples.

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
arsy update [--check]                      # reports the running version; ARSY does not self-update

arsy session list [--workspace-only] [--limit <N>]
arsy session show <SESSION_ID> [--turns] [--evidence]
arsy session export <SESSION_ID> [--out <PATH>] [--include-artifacts]
arsy session rewind <SESSION_ID> --to <EVENT_ID>
arsy session fork <SESSION_ID> [--at <EVENT_ID>]

arsy config explain [KEY] [--source-only]
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

## Design documents

- [Complete architecture specification](docs/INDEX.md)
- [Product requirements](docs/02-product-requirements.md)
- [System architecture](docs/04-system-architecture.md)
- [Rust workspace and technology choices](docs/05-rust-workspace.md)
- [Competitive research](docs/01-competitive-research.md) and [claim ledger](docs/report-source.md)
- [Accepted ADRs](docs/ADR/)

## Target MVP

1. Interactive TUI and non-interactive execution.
2. Official Anthropic and OpenAI API adapters.
3. File, search, patch, shell, and read-only Git operations.
4. Hierarchical instructions through `AGENTS.md`.
5. Approval policy, workspace sandbox, and audit log.
6. Persistent sessions, resume, context compaction, and token/cost summaries.
7. MCP client support for stdio and Streamable HTTP.

Multi-agent orchestration, remote runners, plugin marketplaces, and a default daemon remain gated on a measured, stable single-agent foundation.

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
