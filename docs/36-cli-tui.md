# CLI and TUI surface

The TUI renderer is split by responsibility under `crates/arsy-cli/src/tui/`:
`layout` owns palette and terminal lifecycle, `bar` owns launch/session chrome,
`chat` owns composer and conversation input, `progress` owns tool/provider
cards, `approval` owns approval and plan dialogs, and `provider`/`session` own
their pickers. `tui.rs` remains the public façade and shared terminal
primitives, so the CLI orchestration keeps one stable import surface.

Modern submitted prompts are full-width `›` strips and model output begins with
`✦ Response`; classic keeps the historical `› You` label. Tool, TODO, approval,
and thinking sections stay between those markers, so the transcript has a
visible user/harness boundary even when both contain plain text.

## Implemented integration workflows

The command tables below include roadmap work. The current build provides
`arsy mcp list`, `arsy mcp show <NAME>`, and `arsy hook list` as read-only
inspection commands. MCP inspection reads workspace Claude `.mcp.json`, Codex
`.codex/config.toml`, and the nearest OMP `.omp/mcp.json`; `--source
claude|codex|omp` selects one ecosystem and resolves duplicate names. It preserves
stdio/HTTP transport and explicit enabled state without launching a server.
Hook inspection reads Claude workspace and local settings, preserves lifecycle
events and matchers, and accepts `--event <original-or-canonical-name>`.
Unsupported lifecycle events are labelled unsupported. Other hook ecosystems
and user/enterprise integration configuration are not yet inspected.

An MCP declaration is labelled `not_loaded`: reading a definition is not
connecting. A hook declaration says which it is — `loaded` means it runs on this
workspace's turns.

Native MCP connections are separate from those imported declarations. They are
defined in `arsy.json` under `mcp.server.<name>`, written by `arsy mcp add` and
`arsy mcp remove` and toggled by `arsy mcp enable`/`disable`, with `--scope
user|workspace` selecting the configuration layer that owns the definition. The layer
decides the connection's trust label, so a definition that travels with a
repository is `workspace` rather than `user`. `arsy mcp list` shows those
connections alongside the imported declarations; `--source arsy` narrows to the
native ones. `arsy mcp test <NAME>` is the only command that contacts a server:
it connects, negotiates, discovers, and disconnects, and never invokes a tool.

The client itself supports stdio and Streamable HTTP, bounded bodies and
deadlines, reconnect with bounded backoff and an explicit attempt limit, and
refresh as re-discovery without a teardown. Reconnect and refresh cannot widen
authority: the tools, resources, and prompts adopted on the first connection are
the ceiling, and anything appearing later outside it is reported and rejected.
Authentication failure and a disabled connection are excluded from automatic
retry. Executable hooks are dispatched by the lifecycle engine around each tool call
and at both turn boundaries, in `arsy run` and in the interactive TUI. In the TUI
a hook runs before the operator is asked: it may rewrite or deny a call, and one
that asks for approval opens the approval dialog. A subagent's own calls do not
dispatch them yet. `arsy hook list` reports the effect class, the failure policy,
and whether each declaration is loaded, alongside every file the engine read.

Hooks come from whichever ecosystem the operator already uses, found where
`CLAUDE_CONFIG_DIR` and `CODEX_HOME` put them. `~/.claude/settings.json`
supplies Claude-shaped command hooks; `~/.codex/config.toml` supplies Codex's one
lifecycle callback, `notify`, as `after_turn`; and `~/.arsy/guard.json` is the
same Claude shape under ARSY's own name, for an operator using neither. Only
`type: "command"` runs — a `prompt`, `agent`, or `http` handler is reported
against its file and skipped.

A repository's own hooks — `<workspace>/.arsy/guard.json` and
`<workspace>/.claude/settings.json` and `settings.local.json` — are read but not run until the operator
vouches for that directory:

```json
{
  "project": {
    "/home/you/src/thing": { "trust_level": "trusted" }
  }
}
```

The key is spelled as Codex spells it, and only the enterprise or user layer
may write it: a repository that could vouch for itself would be no gate at all.
Trust is compared on resolved paths, so a symlink beside a vouched-for checkout
does not inherit its trust. Even vouched for, a repository's hook carries
workspace authority — it may deny an operation or ask for approval, never grant
one.

The existing provider subprocess owns its own integrations and permissions.

Human output lists each declaration as a count line and one row per declaration:
its name, transport or matcher, `not loaded`, its source file and ecosystem, the
command or URL a connection would run (hooks report lifecycle, handler types, and
effect), and its trust and mapping level. `--output json` carries the full record.
An empty result names the files that were read and the filters that were applied.

In a TUI build (`cargo build -p arsy-cli --features tui`), use `/help`, `/mcp`,
`/mcp show NAME --source claude`, or `/hooks --event PreToolUse`. The same route
carries the other read-only inspections under their own names: `/settings [KEY]`
for `config explain`, `/doctor`, `/auth` for the credential listing, and
`/compat claude|codex|omp|agents`. Each expands to the CLI command it stands for
and is parsed by the same grammar, so an unsupported argument is refused with the
CLI's diagnostic. Every command on this route is read-only — `auth set`,
`auth login`, and `auth remove` are not reachable from the TUI — so `/model`,
which writes the accepted answer to the user configuration, and the `/mcp`
dialog, which writes only `enabled` toggles and adoptions, are the slash
commands that change state. These work even without provider
authentication. Repeat an inspection to reload its source files. Unknown slash
commands report an error instead of becoming model prompts.

Typing `/` opens a command menu under the composer, one row per command with its
description, narrowed as the line is typed and closed once an argument follows.
Up/Down move the marked row while it is open, and Enter takes the marked command
into the line; a line that already is a command is sent instead, so `/quit` never
has to be chosen from a list. `/help` prints the same table, and both read one
source, so a command cannot appear in one and not the other. The menu is not
offered at the model picker, which collects an answer rather than a command, and
it takes only the rows the terminal has left over the four the composer always
paints, so the input block never outgrows the screen.
Use Up/Down for the last 100 submitted lines when no menu is open, Delete for
forward deletion, and bracketed paste to insert text without submitting pasted
newlines. History is in memory only; multiline paste becomes spaces in the
single-line composer.

`/provider` opens the configured endpoints in the composer's own menu, with a row
to add one and, once something is configured, a row to remove one. Choosing an
endpoint makes it the default. The rows say which endpoint is which: the one
this session resolved at startup is marked in use, and one chosen since then is
marked as taking effect at the next restart. Adding a provider makes it the
default, and without those markers the one it replaced reads as removed rather
than as merely not current. Adding walks one question per field — name,
dialect, base URL, models (one slug, or several separated by commas, the first
being the default), where to keep the credential, then the credential
itself, which is painted as bullets, kept out of the input history, and never
written to the scrollback. Each answer is validated as it is given, an empty
answer leaves the wizard, and nothing reaches the configuration until the last
answer, so an abandoned wizard changes nothing. Removing is confirmed first and
leaves the credential in place; `arsy auth list` still shows it. ARSY edits only
the `provider.endpoint.*` objects it owns and the `provider.default` key; every
other key in the file comes back exactly as it was. A session resolves its
provider at startup, so a change asks
for a restart rather than pretending the running session moved.

A bare `/effort` opens the levels in the composer's own menu, marked at the
current setting: Up/Down move the mark and Enter takes the marked level into the
line, the same keys the command menu answers, and typing narrows the list. A
level name or a list number is still accepted, as is an empty line to keep what
is set. `/effort high` sets the level outright without opening the list. Unknown
answers are rejected with a reason and the list stays open, because an accepted
answer is written to the user configuration. The choice is remembered beside the
model. Unset is the default and sends no
reasoning field at all, so a host without such a model sees the request it always
saw. The two dialects spend it differently: Chat Completions takes the level by
name as `reasoning_effort`, while Messages takes a share of `max_tokens` as a
thinking budget, floored at the 1024 tokens the API requires and omitted when the
output budget cannot hold both the floor and an answer. A scripted `arsy run`
ignores the remembered level and sends no reasoning field, so a pipeline cannot
change behaviour because of an interactive choice made elsewhere.

Esc, Ctrl-C, or Ctrl-D at the effort picker leaves the level unchanged and
returns to the task prompt, as at the model picker.

`/model` lists the models the active endpoint offers, re-read when the picker
opens so one added since startup appears without a restart. A Codex route lists
what the CLI cached instead, and an endpoint that lists no models still takes a
slug as free text. A slug that is not on the list is accepted either way: the
list is what the endpoint advertises, not what it will refuse. Unlike a provider
change, a model change takes effect on the next turn — the endpoint is the same
one the session already resolved.

`/model` reopens the picker. It accepts a list number, a model slug, or an empty
line to keep the current model; anything else — a mistyped slash command, an out
of range number, a slug with whitespace — is rejected with a reason and the
picker stays open, because an accepted answer is also written to the user
configuration. A remembered model is re-validated on read, so a file written by
an older build cannot keep selecting an unusable model. Esc, Ctrl-C, or Ctrl-D at
the picker leaves the model unchanged and returns to the task prompt.

While a turn runs, the composer shows elapsed time and queued follow-ups (up to
16 per running turn), and the active layout re-measures terminal width and
height every 100ms. MCP started/updated/completed events and failure details
appear above it. Esc/Ctrl-C cancels the turn and clears pending follow-ups;
Provider turns currently have a fixed 300-second deadline and a 1 MiB per-event
limit. The terminal is restored on exit and provider processes are cleaned up on
I/O errors. JSON/CI output requires an explicit non-interactive command.

Shift+Tab changes the approval mode immediately and never submits the drafted
task or queues a follow-up. A plan completion opens a full-width `PLAN READY`
card with the structured plan (or the provider's plan text when no structured
steps were recorded); `PageUp`/`PageDown` scroll the preview. Changing mode
while that card is open closes the card and clears pending implementation work.

An operation that still needs authority opens an `APPROVAL REQUIRED` card with
the exact effect, resource scope, reversibility, and reason. `[o]` approves only
that operation. `[r]` records the displayed leaf capability and exact resource
for reuse in this interactive session; it does not switch the session to
`auto`, does not cover another resource, is cleared by `/new` or `/resume`, and
is appended as `approval.granted` to the session event stream. `[d]` denies.
Hook-only approvals omit `[r]` because a hook does not supply a reusable
capability boundary.
OpenAI-compatible requests disable parallel tool calls so the TUI presents one
tool card at a time. A successful duplicate native tool call is answered from
the earlier result, and a repeated successful Git command from the external
Codex route is stopped before another execution when its start event arrives.
A credential is a file beside the user configuration, so preparing a task opens
no platform keyring and costs no unlock prompt; a failed native provider lookup
is cached for the session, and the selected native provider or the logged-in
Codex CLI owns authentication.

File reads show one-based line numbers. Newly created files and text edits show
unified `-`/`+` rows with the anchor line, so the visible cards identify the
exact content that changed instead of only reporting byte counts.

Modern output keeps routine successful reads, existence checks, and Git status
as one-line lifecycle rows. Diffs, searches, MCP results, commands with useful
output, and failures remain typed cards. A running native command keeps a
bounded output tail and `e` expands or collapses it; the complete native result
continues to live in its evidence artifact.

`/mcp` lists every connection the resolved configuration holds: those defined
in `arsy.json` and those Claude Code and Codex declare, which are read live and
connect without being adopted. Each row names the tool and file it came from,
and env and header names without their values. OMP declarations, and those of a
tool switched off with `compat.<tool>.enabled = false`, are listed as inert:
`arsy mcp import [--scope user|workspace]` writes Claude's into `arsy.json`, and
it never repoints a name that is already defined.

In the TUI, `/mcp` with no argument opens a dialog over the same rows. Space or
Enter flips `enabled`: for an `arsy.json` definition in its own file, and for a
Claude or Codex connection as an `enabled` amendment in the user `arsy.json`,
leaving the other tool's files untouched. Turning on a server a repository's
file declares asks first, since it then acts with the operator's authority. On
an inert declaration it asks — showing the full command — before adopting it.
`arsy mcp enable|disable` accepts Claude and Codex names the same way.
`/mcp list` and `/mcp show NAME` remain read-only.

An interactive session holds its MCP connections for as long as it runs. Every
enabled server starts connecting when the session opens, each on its own thread,
and every turn reuses it. At each turn boundary the session is brought in line
with configuration: a server switched off or redefined is disconnected, a
connection that broke under a call is reopened, and a server that failed to
start is reported once and not retried until its definition changes. While a
server connects, the model is offered the tools it published the last time it
connected under the same definition — cached in `~/.arsy/mcp-tools.json`, keyed
by a SHA-256 of the definition including its launch values — and a call to one
waits up to a minute for the connection. A server's own log lines are scrubbed of the values it was
launched with, held, and shown at a turn boundary rather than written as they
arrive: the forwarding thread runs while the composer is being painted, and a
write from it lands wherever the cursor happens to be. `ui.mcp_log` says how
much is shown — `hidden`, `summary` (the default, one line per server with a
count), or `full`. A server that fails to connect is reported whatever the
setting says. The boundary is the turn, so a line written while a turn runs
appears when the next one starts. `arsy run` connects for its single turn and
writes those lines to stderr as they arrive, each prefixed with the name of the
server that wrote it.

The launch card includes the session's opening approval mode and is reprinted
when the model changes. Later mode changes are historical transcript strips;
the live footer always carries the active mode, and the explanatory line below
the card states what that mode allows. Completed turns report real changed-file,
displayed-rule, and durable-event counts before `resume with /resume`. On a
terminal that speaks the Kitty graphics protocol the mark is drawn as an image
rasterised from `assets/logo.svg`; every other terminal keeps the half-block
mark. Workspace paths under `$HOME` are displayed with `~`.

Checks: `cargo test -p arsy-cli --features tui`,
`cargo test -p arsy-code --test compat_golden`, and
`python3 crates/arsy-cli/tests/tui_smoke.py target/debug/arsy` (Unix, TUI build).
Any workspace-wide cargo command rebuilds `target/debug/arsy` without the `tui`
feature, so run `cargo build -p arsy-cli --features tui` immediately before the
smoke test; it reports a stale binary rather than timing out on it.

## Invocation

The executable is `arsy`. UTF-8 is required for task text and machine output. Arguments use platform-native paths; internally they are canonicalized before policy evaluation.

Global flags apply before or after a subcommand:

| Flag | Value/default | Description |
|---|---|---|
| `--workspace <PATH>` | current directory | workspace root |
| `--config <PATH>` | discovered native config | use one additional session-scoped config file; it cannot weaken policy |
| `--provider <ID>` | resolved default | select an allowed provider |
| `--model <ID>` | resolved default | select an allowed model |
| `--output <MODE>` | `human` on a TTY, `ci` otherwise | `human`, `json`, or `ci` |
| `--no-color` | false | disable ANSI styling; equivalent to `ui.color = "never"` |
| `--debug` | false | trace the agent loop to stderr as JSON lines: requests, normalized model events, tool results, retries, and turn transitions. Redacted like every other sink, and absent entirely when unset |
| `--help` | — | print help and exit |
| `--version` | — | print version and target, then exit |

Unknown flags, missing arguments, invalid UTF-8, and invalid enum values are usage errors and never start a session.

## Commands

Every command accepts the global flags above and honours the output modes and
exit codes below. **Availability** retains the original design-slice number
encoded by current diagnostics; it is not a phase number in the source-backed
[`31-roadmap.md`](31-roadmap.md). A command listed here but not yet available
exits `2` with an `ARSY-SCH-*` diagnostic naming that design slice, never a
generic parse error.

Commands whose name is `list`, `show`, `explain`, `inspect`, or `test`, plus `doctor` and `eval`, are
read-only: they never mutate the workspace, session history, or stored configuration.

### Session

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy` | none | global flags | open the interactive TUI in the workspace | 2 |
| `arsy run <TASK>` | one required task string; `-` reads it from stdin | `--image <PATH>` plus global flags | execute one task non-interactively and exit at its terminal state; `--image` attaches one png, jpeg, gif, or webp of at most 5 MiB, and a provider that cannot read one refuses the turn rather than dropping it | 1 |
| `arsy resume <SESSION_ID>` | one required canonical session ID | `--follow` plus global flags | resume an existing session; follow new events until terminal when requested | 1 |
| `arsy review [REVISION]` | optional Git revision; omitted means `HEAD`, so the working tree | `--base <REVISION>`, `--strict` plus global flags | report what changed, the verification depth it implies, and findings that name a file; `--strict` makes any finding a non-zero exit | 6 |
| `arsy session list` | none | `--workspace-only`, `--limit <N>` | list session IDs with workspace, status, start time, and token totals | 1 |
| `arsy session show <SESSION_ID>` | one required session ID | `--turns`, `--evidence` | show turns, recorded evidence, approvals, and totals for one session | 1 |
| `arsy session export <SESSION_ID>` | one required session ID | `--out <PATH>`, `--include-artifacts` | export canonical events as JSONL for audit or forensic review | 1 |
| `arsy session rewind <SESSION_ID>` | one required session ID | `--to <EVENT_ID>` required | create a new branch pointing at an earlier event; never truncates history | 1 |
| `arsy session fork <SESSION_ID>` | one required session ID | `--at <EVENT_ID>` | start a new session recording ancestry from an existing one | 1 |

### Configuration and policy

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy config explain [KEY]` | optional dotted key; omitted explains every key | `--source-only` | show the effective value, the layer that supplied it, the merge strategy, and the rejected candidates | 1 |
| `arsy compat explain <ECOSYSTEM>` | one of `claude`, `codex`, `omp` | `--loss-only` | show discovered sources, precedence, canonical mapping, and the loss report | 5 |
| `arsy policy explain <OPERATION>` | one canonical operation kind | `--resource <REF>`, `--actor <ID>` | evaluate a policy query and print the decision, deciding rule, and policy source without executing anything | 1 |

### Credentials and models

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy auth set <PROVIDER>` | one configured provider ID | `--handle <NAME>` | read a secret from a no-echo prompt, or from stdin when piped, store it in a `0600` file beside the user configuration, and print only the resulting handle. `--handle` names that file; a name without a `.key` suffix is given one, so the handle reads `secret://file/<name>.key` | 1 |
| `arsy auth login <PROVIDER>` | one configured provider ID | global flags | sign in through the OAuth client the provider's configuration names, using the device grant when it offers one and the authorization-code grant with PKCE otherwise, and store the resulting token set under the provider's handle | 1 |
| `arsy auth list` | none | global flags | list stored credential handles with provider, creation time, and last use; never the secret value | 1 |
| `arsy auth remove <HANDLE>` | one required handle | `--force` | delete a stored credential and report the configuration keys that referenced it | 1 |
| `arsy provider list` | none | `--all` | list providers resolved as allowed, with the ceiling that narrowed them | 1 |
| `arsy model list` | none | `--provider <ID>`, `--capability <NAME>` | list allowed models with tri-state capabilities, capability source, and observation date | 2 |

### Connections

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy mcp list` | none | global flags | list configured MCP connections, transports, trust labels, and enabled state without connecting | 5 |
| `arsy mcp add <NAME>` | one connection name | `--transport <stdio\|http>`, `--command`, `--url`, `--scope <user\|workspace>` | write a connection definition; `--scope` defaults to `user` | 5 |
| `arsy mcp remove <NAME>` | one connection name | `--scope <user\|workspace>` | remove a connection definition from the named scope | 5 |
| `arsy mcp enable <NAME>` / `arsy mcp disable <NAME>` | one connection name | `--scope <user\|workspace>` | toggle a connection without deleting its definition | 5 |
| `arsy mcp test <NAME>` | one connection name | `--timeout <SECONDS>` | connect, negotiate capabilities, and disconnect; never invokes a tool | 5 |
| `arsy mcp reconnect <NAME>` | one connection name, or none with `--all` | `--all`, `--timeout <SECONDS>` | restore a dropped live connection and re-apply its capability ceiling | 5 |
| `arsy mcp refresh <NAME>` | one connection name, or none with `--all` | `--all` | re-run tool, resource, and prompt discovery without tearing the connection down | 5 |

### Extensions

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy plugin list` | none | `--capabilities` | list installed plugins with version, signature status, and granted capabilities | 8 |
| `arsy plugin install <SOURCE>` | one path or registry reference | `--scope <user\|workspace>` | display the requested capabilities and install only on explicit confirmation | 8 |
| `arsy plugin inspect <ID>` | one plugin ID | global flags | show the manifest, requested and granted capabilities, limits, and publisher identity | 8 |
| `arsy plugin remove <ID>` | one plugin ID | `--force` | uninstall a plugin and revoke its grants | 8 |
| `arsy plugin refresh [ID]` | optional plugin ID; omitted refreshes every source | `--dry-run` | reload plugins, skills, and hooks from their sources, effective at the next turn boundary | 8 |
| `arsy skill list` | none | `--source` | list loaded skills with their originating layer and authority class | 5 |
| `arsy hook list` | none | `--event <NAME>` | list registered hooks with lifecycle event, declared effect class, and origin | 8 |

### Evidence

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy artifact show <REF>` | one `artifact://` reference | `--max-bytes <N>` | render a bounded, redacted excerpt with the artifact's metadata | 1 |
| `arsy artifact export <REF>` | one `artifact://` reference | `--out <PATH>` required | write the artifact to a caller-named path and report what redaction removed | 1 |

### Maintenance

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy doctor` | none | `--strict` | check configuration, credentials by handle, sandbox backends, Git, providers without a billable request, and release provenance; `--strict` turns warnings into failure | 1 |
| `arsy migrate` | none; the session store is the only thing with a migration chain | `--apply`, `--backup <PATH>` | report the planned migration and its loss report; `--apply` is required to write, and takes a verified backup first | 1 |
| `arsy memory list` | none | `--scope <SCOPE>`, `--all` | what this workspace remembers; `--all` includes superseded and revoked records | 9 |
| `arsy memory remember <CLAIM>` | one required claim | `--scope <SCOPE>` | record a durable claim, stored as an artifact like any other evidence | 9 |
| `arsy memory forget <ID>` | one required memory ID | `--to <REASON>` | withdraw a record, keeping the tombstone and its reason | 9 |
| `arsy gc` | none | `--apply`, `--retention <DURATION>` | report artifacts unreachable and past retention; `--apply` is required to delete | 1 |
| `arsy serve` | none | `--protocol <mcp\|acp>`, `--transport stdio` | offer operations as MCP tools, or speak ACP to an editor; stdio only, because this is never a background daemon | 1 |
| `arsy eval <SUITE>` | one suite path or ID | `--trials <N>`, `--out <PATH>` | run an evaluation suite and report outcome, efficiency, and safety metrics | 1 |
| `arsy completions <SHELL>` | one of `bash`, `zsh`, `fish`, `powershell` | none | print a shell completion script to standard output | 1 |

### Command rules

A group name used without a subcommand — `arsy mcp`, `arsy session`, `arsy auth`, `arsy plugin`,
`arsy artifact`, `arsy config`, `arsy compat`, `arsy policy` — prints its help and exits with usage
status. `--base` is invalid unless `REVISION` is absent. `resume --follow` is implied in an
interactive TTY and otherwise defaults off.

Bare `arsy` prefers a configured provider endpoint and falls back to a logged-in Codex CLI when
nothing is configured, so `/model` offers Codex's cached list on that route and takes a slug as
free text on a configured one. The remembered choice is stored as `provider/model` and only applies
to the provider it was chosen for.

`arsy auth login` prints the URL to visit rather than opening a browser, because an operator working
over SSH is not looking at a browser on the machine that ran the command.

`arsy auth set` never accepts a secret as an argument, because arguments reach the process list and
shell history. The credential is a file readable by its owner alone, written at that mode rather than
narrowed afterwards; when it cannot be written there the command fails. No command prints a stored
secret in any output mode.

`arsy auth remove` on a `secret://os/...` handle drops the catalog record and nothing else. The
platform keyring was withdrawn (see [ADR-0013](ADR/0013-file-only-credential-store.md)), so ARSY can
neither read that entry nor delete it, and saying otherwise would be a claim it cannot make good on.

`arsy mcp reconnect` repairs a live connection and re-applies its capability ceiling; `arsy mcp test`
is a separate probe that connects and disconnects without touching the session. Neither accepts a
capability the connection did not already hold: a tool that appears only after reconnecting and falls
outside the ceiling is rejected, not adopted.

`arsy plugin refresh` reloads plugins, skills, and hooks, takes effect at the next turn boundary
rather than immediately, and refuses any source whose manifest requests wider capabilities than were
approved at install. `--dry-run` reports what would change and loads nothing.

`arsy migrate` and `arsy gc` report without writing unless `--apply` is given, so a forgotten flag
cannot destroy data. `arsy migrate` takes a verified backup before applying and leaves the original
store openable if it fails.

`arsy mcp add` writes to the user `arsy.json` by default and to `.arsy/arsy.json` under
`--scope workspace`. A definition written at either scope remains untrusted content: it declares a
connection, and grants no capability.

ARSY has no update command. Updates and rollbacks are handled by the installation channel, as
specified in [distribution](34-distribution.md).

## TUI behavior

The status line carries the model route, the reasoning effort (`effort:—` when
unset), and the workspace path, with the checked-out branch right-aligned at the
far edge so it holds its column while the fields to its left change length. A
narrow terminal gives the fields up in the order they can be spared: the
workspace path shrinks to its last segments behind a `…/`, then disappears, and
only then is the branch dropped. The branch is never shortened, because half a
branch name reads as a different branch. The branch is read from `.git/HEAD` once per prompt, so a
checkout made in another terminal appears on the next line rather than at the
next restart.

The TUI has a session timeline, task input, status line, evidence/diagnostic detail, and an approval view. At startup it detects a logged-in Codex installation through `codex login status`, then asks for a model; an empty selection uses the Codex default. Codex credentials and configuration remain owned by Codex and are never copied into ARSY. Entering a task runs it through Codex in read-only mode and returns to the task prompt; `:quit` or end-of-file exits. It displays the active workspace, model route, session ID, achieved sandbox assurance, token/cost totals, and whether the result is degraded. Keyboard actions and screen-reader labels must expose every action available by pointer.

An approval view identifies the operation, canonical resource, exact scope, risk, policy source, expiry, and proposed assurance. The only decisions are deny, approve this operation, or approve the displayed bounded rule. Closing the view denies; repository content and model output cannot preselect approval.

The TUI renders canonical events and may reconnect from its last event cursor. It never owns session truth and never hides a terminal diagnostic behind a transient notification.

## Output modes

| Mode | Standard output | Standard error | Presentation |
|---|---|---|---|
| `human` | final answer or requested listing | progress, approvals, warnings, diagnostics | localized prose, optional ANSI, progress redraw allowed |
| `json` | UTF-8 NDJSON records | bootstrap failures before JSON initialization only | no ANSI; each record has `schema_version`, `type`, `sequence`, `session_id`, and typed payload |
| `ci` | final answer or listing | stable `ARSY <SEVERITY> <CODE> <MESSAGE>` lines | no ANSI, spinner, cursor control, localization, or interactive prompt |

JSON record types are `event`, `result`, and `diagnostic`. Exactly one terminal `result` is emitted after initialization, even on failure; diagnostics use the fields in [the stable taxonomy](33-diagnostics.md). Machine modes never mix human prose into structured stdout. Secret values are redacted in every mode.

## Exit codes

The process returns the class-specific code for the terminal diagnostic. Warnings with a valid degraded result return `0` unless `doctor --strict` is active.

| Exit | Meaning | Diagnostic classes |
|---|---|---|
| `0` | completed, possibly with reported warnings | none |
| `2` | invalid CLI/config/schema input | `SCH` |
| `3` | policy denial or approval unavailable | `POL` |
| `4` | required sandbox assurance unavailable | `SBX` |
| `5` | provider or protocol failure | `PRV`, `PRT` |
| `6` | operation, tool, edit, or stale-write failure | `EXE`, `TLS`, `EDT`, `STL` |
| `7` | verification failed; completion blocked | `VER` |
| `8` | compaction, coordination, storage, or internal integrity failure | `CMP`, `CRD` |
| `9` | retrieval, context, planning, or unsupported model result | `RET`, `CTX`, `PLN`, `MDL` |
| `10` | requested interface cannot present a usable result | `UIX` |
| `130` | interrupted by the user | terminal interrupt diagnostic |

If several failures contribute, the terminal/primary diagnostic selects the exit code and contributing diagnostics remain in output. Signals and Windows control events are normalized to `130` only for an acknowledged user interrupt.

## Non-interactive and approval behavior

With no TTY, bare `arsy` fails with exit `2` and directs the caller to `arsy run`; it never guesses a task. `run`, machine-output modes, and non-TTY `resume --follow` never display or wait on a terminal approval prompt.

When policy returns `ask` and no authenticated approval channel is attached, ARSY emits an `ARSY-POL-*` diagnostic naming the rule and required approval, records the operation as not executed, and exits `3`. There is no implicit approval, weaker sandbox fallback, or environment variable that bypasses this rule. Piped stdin supplies task text only and conveys no authority.
