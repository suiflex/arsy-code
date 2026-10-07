# Harness core: what is built, and how to extend it

This is the *as-built* description of the layer between a model and the
workspace. [`04-system-architecture.md`](04-system-architecture.md) describes
the target the project is heading for; this describes what is in `main` now, so
a contributor does not have to reverse-engineer it.

Everything here lives in `crates/arsy-code/src/agent/`.

## The one path

There is exactly one route from a model tool call to an effect. Nothing —
not the TUI, not `arsy run`, not `arsy serve` — has another.

```text
model tool call            name + JSON arguments as the provider sent them
      │
      │  ToolRuntime::prepare  →  agent::TOOLS lookup, argument translation
      ▼
OperationRequest           kind, actor, and the capability requirements the
      │                    call needs over concrete resources
      │  ToolRuntime::authorize  →  RuleSet::evaluate, once per requirement
      ▼
Authorization              Allowed(grants) | NeedsApproval{..} | Denied(reason)
      │
      │  ToolRuntime::dispatch  →  OperationRegistry::dispatch
      │                            (schema check, grant check, execute)
      ▼
OperationOutcome           result and evidence, stored as artifacts
      │
      │  ToolRuntime::render  →  read the artifact back, render, truncate
      ▼
ToolResult                 { success, output, changed_files, metadata, artifact }
```

Two properties follow from having one path, and both are worth protecting:

- **A capability decision is made once.** Adding a front end cannot add a way
  around policy, because a front end only ever produces an `OperationRequest`.
- **A tool call is replayable.** Every executor writes its result to the
  artifact store rather than returning it inline, so `ToolResult.artifact`
  points at the whole thing even after the transcript has been trimmed.

## Layers

| Concern | Where | Note |
|---|---|---|
| Model-facing tools | `agent::TOOLS` | name, description, JSON Schema, argument translation. No policy, no state, no I/O. |
| Execution + authority | `agent::ToolRuntime` | the diagram above |
| Transcript budget | `agent::budget` | which observations stay verbatim |
| Instruction loading | `agent::instructions` | which Markdown reaches the model |
| File operations | `agent::fsops` | `fs.read/list/write/create/edit/delete/move` |
| Patch | `agent::patch` | the `*** Begin Patch` dialect |
| Search | `agent::searchops` | `search.files`, `search.text` |
| Shell | `crate::process` | `process.exec`, sandbox-aware, workspace cwd |
| Git | `crate::git` | `git.status/diff/log/blame` |
| Path confinement | `crate::resource::Workspace` | the only way to touch a workspace file |
| Registry | `crate::operations::registry` | the one place executors are registered |

## Adding a tool

1. **Write an executor** implementing `OperationExecutor`. Its contract
   declares the capability actions it needs, whether it is idempotent, whether
   it is **reversible**, and its input schema. Store the result as an artifact
   and return its reference as `OperationOutcome::value`.
2. **Register it** in `crate::operations::registry`. Nothing else registers
   executors, so `arsy policy explain`, `arsy serve`, and a turn cannot end up
   with different sets.
3. **Add a `Tool`** to `agent::TOOLS` if the model should see it: a name, a
   description, a JSON Schema, and a `translate` function from the model's
   arguments to the operation's input.
4. **Render its result** in `agent::present` if the default pretty-printed
   JSON is not what the model should read.

`ToolRuntime::schemas` filters by what the registry can actually dispatch, so a
tool whose executor is not registered is never offered.

### Reversible is not the same as idempotent

`Idempotency` says whether *repeating* a call repeats its effect.
`OperationContract::reversible` says whether the effect can be undone.

Policy raises an irreversible call to approval however permissive a rule is.
Equating the two would make every edit need a human, which is how an operator
learns to say yes without reading. Rewriting a file is effectful and
reversible — version control still has the previous content. Removing a file,
or running a command, is neither.

## Authority

Policy decides every call; a front end only chooses what to do about the
answer.

| Decision | TUI | `arsy run` | `arsy serve` |
|---|---|---|---|
| `Allowed` | runs, no prompt | runs | runs |
| `NeedsApproval` | prompts, and a "yes" mints a grant over exactly the resource shown | refused, reported to the model | refused |
| `Denied` | refused, reported to the model | refused | JSON-RPC `-32020` |

The default configuration is `policy.default_effect = "ask"` on every action,
so a first run confirms everything. An operator who wants edits without prompts
writes `allow` rules for `fs.write`; deletions and shell commands still confirm,
because they are declared irreversible.

`ApprovalRequest::grant` produces a grant over the literal resource, with
delegation depth zero. An approval is never a mode.

## Context

The system prompt is rebuilt each round from:

```text
HARNESS_INSTRUCTIONS               what the tools are for
  + ~/.codex/AGENTS.md, ~/.claude/CLAUDE.md   the operator's own, when enabled
  + AGENTS.md / CLAUDE.md          root first, then each directory down to cwd
  + task context                   when a caller supplies one
```

compiled through `arsy_kernel::prompt::compile`, so ordering follows the model
family and secrets are redacted on the way out.

Discovery is deliberately narrow — on the ancestor path only, each directory
contributes its agents file (`AGENTS.override.md`, else `AGENTS.md`, else
`.arsy/AGENTS.md`) and its `CLAUDE.md` unless that is a copy, or `GEMINI.md` when
it has neither. `README.md` and `docs/*.md`
are *not* injected; they are documentation the model reads with `search.text`
and `fs.read` when it needs them. The objective is relevant context, not
maximum context.

MCP tools follow the same rule. A connected server's full schemas are not
sent until the model asks for that server with `mcp.load`, whose description
names each server and its tool names; from then on, for the rest of the
runtime, that server's tools are offered beside the built-ins. A session with
a few busy servers otherwise spent tens of thousands of tokens per request on
schemas the task never used. Loading touches nothing and needs no authority;
calling a loaded tool is authorized like any other.

The transcript is trimmed to a token budget before each request by
`agent::budget::trim`. Tool results are never removed — that would break the
call/result pairing every provider requires — their bodies are replaced by a
stub naming the call and its size. The ranking comes from
`arsy_kernel::context::ContextView::select`, so there is one selector that can
explain its own omissions rather than two that might disagree.

## Repository awareness

`Workspace` owns a cap-std `Dir` for the root. Confinement is two independent
checks: a lexical one that refuses `..`, absolute paths, and prefixes, and the
syscall-level one cap-std performs, which is what catches a symlink whose name
looks local. Nothing else in the crate opens a workspace path.

An additional directory is one the operator added beside the workspace, with
`--add-dir` or `execution.additional_directories` (user or enterprise layer
only — a repository cannot widen its own reach). `resource::locate` sorts a
path into the workspace, an added directory (opened as its own `Workspace`, so
both confinement checks still apply inside it), or neither. File and search
operations work in an added directory as in the workspace, and the approval
mode decides as it does there. A path outside both is turned into a question
by `ToolRuntime::authorize` whatever policy allows, and its executor runs it
only under a grant that names that exact path (`CapabilityGrant::names_exactly`):

| Mode | added directory | anywhere else |
|---|---|---|
| Plan | read; writes refused | read asks; writes refused |
| Default | read; edits ask | asks |
| Accept Edits / Auto | read and edit | asks |
| Don't Ask | read; edits refused | refused |
| Bypass | everything | everything |

Approving such a call "always" (`[r]`) allows its repository — the nearest
ancestor holding `.git`, never the home directory or a root, else the path's
own directory — for the rest of the session: for reading when every outside
requirement was a read, otherwise as an added directory. `ApprovalCell`
carries it into each later turn's runtime.

`apply_patch`, git, and the shell stay workspace-only. The prompt lists the
added directories, and an effect outside the workspace is recorded as
`file:<absolute path>`.

`resource::walk` is the one traversal: `.gitignore` filters plus a skip of
`.arsy` (configuration and `.arsy/state/` alike), so a listing, a text search, and a file find agree about what the
workspace contains.

## What is not here yet

- Cancellation is the TUI's alone. Esc or Ctrl-C sets `Turn::interrupted`; the
  remaining calls of that round are still *answered* — a provider that sent a
  call and never sees its result rejects the next request — but nothing further
  runs. A call already in flight finishes, because killing it mid-write is how
  a file is left half-edited, so interrupting a long `bash` waits for its
  deadline. `arsy run` has no interrupt source: a scripted turn ends at its
  round limit or a tool's deadline. `ToolRuntime` holds no cancellation flag of
  its own; a second mechanism that the loop did not consult would be a knob
  that looks like it works.
- The context budget is a constant, not a per-model window, and it counts the
  transcript only — the system prompt and the tool schemas are not in it. The
  constant carries the slack.
- `fs.write` and `apply_patch` write in place: truncate, then write. A failure
  mid-write leaves a truncated file. `fs.edit` does not have this problem —
  it goes through `edit::apply_unversioned`, which stages into a temporary
  directory and renames. Moving the other two onto the same path is the fix,
  and is not done.
- `apply_patch` is not atomic across files. A hunk that fails in the third file
  leaves the first two written; the error says which, so the model can re-read
  before retrying.
- Reachability outside the core path is mixed. Memory recall, orchestration,
  telemetry, repository mapping, LSP-backed intelligence, DAP, MCP, hooks, and
  WASM plugin operations have reachable slices, and several integrations are
  still absent from interactive or child turns. The source-audited status
  table and dependency order are in [`31-roadmap.md`](31-roadmap.md).

## Delegation, as built

`task.spawn` records a child in the session's task graph, admits it against
bounded slots, hands it to a worker thread, and returns its task and attempt
ids. The parent keeps its turn and reads the answer back with `task.wait`,
`task.result`, or `task.status`, stops one with `task.cancel`, and narrows a
running one with `task.send`. A turn that never delegates still gets
`task.criterion`, because committing to what done means is not delegation.

Each child runs in a view of its own. A reader gets an immutable checkout
pinned to a revision; an isolated writer gets a Git worktree on its own
branch, and `task.integrate` is the only way its work reaches the workspace.
The harness commits the writer's view for it, so a writer needs no process
authority to produce a revision an integrator can name.

Cancellation reaches a child between rounds and between tool calls, which are
the points where stopping leaves a workspace the child can describe. The
durable request is recorded before the flag is set, so a process that dies
between the two comes back knowing the attempt was told to stop. A call
already in flight still finishes, for the reason above.

`ToolRuntime` itself holds no cancellation flag; the token lives on the
attempt and is polled by the child loop, not by the runtime.

## Verification, as built

A completion proof is rebuilt from recorded operations, never stored and
trusted. `arsy verify <session>` reads the same event stream from outside the
run and exits nonzero unless every required criterion is met by valid, fresh,
attributable evidence — refusing a pass from a different command, another
task, a superseded attempt, a missing artifact, or another revision. The graph
will not promote a task to verified without such a proof, so "the turn
finished" cannot stand in for "the work holds".
