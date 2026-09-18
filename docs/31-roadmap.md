# ARSY CODE engineering roadmap

Status: source-backed roadmap, 2026-09-15. Current-state claims were checked
against repository revision `7656e6e8d6252c775a3744db0e2252c19c79d728`.

## Purpose

ARSY CODE is evolving into a local, auditable, model-independent
software-engineering agent harness with durable orchestration, safe autonomy,
multi-agent execution, and proof-carrying completion.

The product advantage is not the number of tools. It is the ability to perform
software-engineering work while preserving a reviewable chain from user intent,
through policy and authority, to each effect and the evidence supporting the
result. The next architectural step is therefore a durable multi-agent runtime,
not another broad tool expansion.

This document replaces the earlier implementation-phase checklist. It is the
delivery roadmap; the numbered design documents and accepted ADRs remain the
behavioral source of truth. [`37-harness-core.md`](37-harness-core.md) describes
the current harness path in more detail.

## Status vocabulary

| Status | Meaning |
|---|---|
| **SHIPPED** | Reachable through a supported CLI, TUI, service, or model-tool path and covered by executable checks. |
| **PARTIAL** | A useful end-to-end slice is reachable, but a material behavior or safety property is absent. |
| **PRIMITIVE** | Types or an isolated implementation exist, but the product path does not yet use them completely. |
| **PLANNED** | No sufficient implementation exists at this revision. |
| **DEFERRED** | Deliberately excluded until a named dependency or benchmark justifies it. |

“Implemented” in a source file is not enough for **SHIPPED**. Operational
release status additionally requires an enabled workflow and evidence that the
channel has published successfully.

## Product thesis and strategic principles

ARSY should preserve one authority path and extend the structures it already
has:

1. Model providers produce normalized requests and never receive a workspace
   mutation handle.
2. Every effect remains a typed operation authorized against canonical
   capabilities and resolved resources.
3. `TaskGraph` becomes the durable scheduler rather than introducing a second
   orchestration database.
4. Child authority is an attenuation of recorded parent authority.
5. Parallel readers use revision-bound snapshots; every mutable view has one
   writer.
6. Agent state and communication are events; the TUI is a projection.
7. Completion is a claim over acceptance criteria and evidence, not a model
   utterance or a successful provider response.
8. Provider-neutral roles and measured model profiles drive routing.
9. New intelligence features must improve held-out evaluations before becoming
   defaults.
10. Unknown capability, stale evidence, and uncertain recovery state remain
    explicit.

## Current state

The source audit distinguishes tool concurrency, background processes, and
background agents. They are different capabilities.

### Audit scope and evidence

The audit followed the reachable call paths as well as standalone kernel types:

- Agent execution and tools: `crates/arsy-code/src/agent/`,
  `crates/arsy-code/src/operations.rs`, `crates/arsy-cli/src/lib.rs`, and
  `crates/arsy-cli/src/subagent.rs`.
- Authority and state: `crates/arsy-kernel/src/{operation,capability,policy,event,
  service,orchestration,todo,memory,context}.rs`, plus SQLite storage and CLI
  session/transcript modules.
- Workspaces and execution: `crates/arsy-code/src/{workspace,process,procsession,
  sandbox,edit}.rs` and `crates/arsy-pty/src/lib.rs`.
- Intelligence and extension surfaces: `crates/arsy-code/src/{repomap,graph,syntax,
  intelligence,lsp,dap,extension}.rs`, agent code/debug/discovery/MCP/plugin
  operations, and CLI MCP/hook/extension integration.
- Models and quality: kernel provider/profile/routing/telemetry modules,
  `crates/arsy-cli/src/{provider,telemetry,eval}.rs`, fixtures, tests, and benchmarks.
- Product operation: TUI/CLI source, npm packages/scripts, installers, disabled
  workflows, `.github/workflows/README.md`, and distribution documentation.

Competitor statements are deliberately bounded. Claude Code behavior comes
from its [official subagent documentation](https://code.claude.com/docs/en/sub-agents);
Codex behavior comes from [official OpenAI multi-agent documentation](https://learn.chatgpt.com/docs/agent-configuration/subagents).
oh-my-openagent and oh-my-pi statements come from their public repositories at
[code-yeongyu/oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent)
and [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi). The older pinned
source comparison remains in [`report-source.md`](report-source.md); benchmark
claims must pin new revisions independently.

### Runtime, authority, and execution

| Capability | Status | Source-backed current behavior |
|---|---|---|
| Model tool → operation → policy → capability → executor | **SHIPPED** | `agent::ToolRuntime` prepares and authorizes `OperationRequest`; `OperationRegistry` validates contracts and grants before dispatch. See `crates/arsy-code/src/agent/mod.rs` and `crates/arsy-code/src/operations.rs`. |
| Typed capabilities and policy | **SHIPPED** | Grants bind actor, action, resource scope, expiry, and delegation depth. Explicit denial wins in every approval mode. See `crates/arsy-kernel/src/capability.rs` and `policy.rs`. |
| Plan Mode enforcement | **SHIPPED** | `ExecutionMode::Plan` refuses workspace mutation and command execution before ordinary policy dispatch. This ceiling is propagated into synchronous child runtimes. |
| Approval modes | **PARTIAL** | `default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, and `bypassPermissions` exist. `auto` and `bypassPermissions` both approve policy `NeedsApproval` today; neither overrides an explicit denial. No independent safety reviewer distinguishes them. See `crates/arsy-cli/src/approval.rs`. |
| Bounded tool-call concurrency | **SHIPPED** | `ToolRuntime::invoke_batch` runs operation contracts marked parallel. The turn loop only batches when hooks and subagent supervision are absent. This is tool concurrency, not agent concurrency. |
| Process lifecycle and PTY | **PARTIAL** | `process.start/poll/write/stop` manage process-wide sessions; they are not restart-durable. PTY support is Unix-only through the separately scoped `arsy-pty` crate. Background processes are not background agents. |
| Atomic edits | **PARTIAL** | `fs.edit` uses staged application, while `fs.write` and `apply_patch` can leave partial results; multi-file patch application is not atomic. This remains a safety prerequisite for broad autonomy. |

### State, planning, validation, and agents

| Capability | Status | Source-backed current behavior |
|---|---|---|
| Event-sourced sessions | **SHIPPED** | Stable event envelopes, SQLite event storage, artifact CAS, turns, and projections exist. Session resume closes interrupted turns and can replay task state. |
| Transcript restoration and context compaction | **PARTIAL** | Provider-safe transcript records are persisted and restored; context selection compacts the view without deleting canonical history. TUI reconstruction reads a bounded event page and completed turns, so it is not yet general task recovery. |
| `plan.*` | **PARTIAL** | A structured plan survives registry rebuilds and transcript trimming within the process. State is held in a process-global map and is lost on restart: it is scratch planning by design. `plan.commit` is the explicit transition that turns each uncommitted step into a durable TODO, once per step, and records the TODO id against the step. |
| `todo.*` | **SHIPPED** | TODO mutations append to the session event stream and survive restart. TODOs are durable user-visible commitments, separate from plans and execution nodes. |
| `validate.*` | **PARTIAL** | `validate.record` verifies that a cited `ProcessResult` artifact ran the named command and exited successfully, then writes the record to the session event stream through `arsy_kernel::validation`. A record carries the evidence artifact, a command digest, the operation, the task and attempt, the grant ids the call held, and the workspace revision; `validate.status` reports `unvalidated`, `passed`, `failed`, or `stale` against the revision it is asked about, and says whether the log is durable. Acceptance criteria are still not modelled, so a record is not yet bound to one. |
| `TaskGraph` | **PARTIAL** | Durable nodes, dependencies, state transitions, cycle detection, join predicates, and attenuated child grants exist, along with versioned `TaskAttempt` records (parent task/attempt, retry lineage, role, assignee, model decision, base revision, authority snapshot, reservation, usage, lease epoch, terminal reason, result, evidence), atomic parent/child budget reservation and settlement, command revalidation after store contention, lease-epoch fencing of late results, paged replay, and a `verified` state distinct from `completed`. Typed result artifacts, durable worker descriptors, and active scheduler ownership do not. |
| `task.spawn` | **PARTIAL** | A child gets a graph node, lease, budget field, and attenuated read/process authority. The budget is not fully enforced or settled. `Supervisor::spawn` calls the child loop inline and returns its answer, so the parent blocks. Child execution has no durable transcript/result contract and does not use an immutable workspace snapshot. |
| Restart recovery | **PARTIAL** | Expired/running task leases can be reclaimed, and the attempt that held one is retired rather than left able to commit. A rebuilt graph restores attempt state, authority snapshot, reservation, usage, and terminal reason, and `TaskGraph::narrow_authority` re-derives a recovered task's grants against current policy, narrowing only. Resume still rebuilds the runtime from current workspace policy/model rather than from the attempt descriptor. |
| Workspace isolation | **PRIMITIVE** | `WorkspaceRequirement::{ReadOnlySnapshot, IsolatedWriter}` and `WorkspaceCoordinator` exist. The coordinator creates detached Git worktrees or copied snapshots, tracks in-memory leases, and can merge a writer revision. CLI agents do not use it; its ownership is not durable. |
| Agent messaging and supervision | **PLANNED** | No durable mailbox, steering, task result, wait API, or Agent Hub exists. Observer primitives consume redacted projections, but no complete product supervision path is wired. |

### Intelligence, extensions, models, and quality

| Capability | Status | Source-backed current behavior |
|---|---|---|
| Repository discovery and map | **SHIPPED** | Bounded discovery identifies manifests; `repo.map` writes a hash-incremental `.arsy/repo-map.json`. Generic incremental repository mapping should not be rebuilt. |
| Code intelligence | **PARTIAL** | Model tools route through configured LSP, a Rust tree-sitter/graph implementation, then text fallback. LSP rename is transactional. The native graph currently models Rust definitions/imports rather than a broad call/reference graph. |
| DAP | **PARTIAL** | `debug.run` performs one bounded DAP run and records protocol/stack/local evidence. It is not a persistent interactive debugging session. |
| MCP, hooks, and WASM plugins | **PARTIAL** | Native MCP operations, hook execution, and capability-limited WASM plugin operations exist. Hook execution is not consistent across scripted, interactive, and child turns. Imported declarations are not automatically loaded. |
| Memory model | **PARTIAL** | Event-backed records model scope, provenance, confidence, authority origin, expiry, supersession, revocation, and artifact-backed claims; bounded repository recall is wired. Automatic candidate extraction and trust-aware curation are absent. |
| Provider abstraction and model profiles | **SHIPPED/PARTIAL** | Provider transport is separated from versioned model capability profiles. Declared/probed/override states exist; probe caching is in memory. |
| Model routing | **PRIMITIVE** | Routing filters provider/model/residency/capability constraints and can rank measured cost, latency, and failures. The CLI supplies fresh empty observations and a generic task preference, so historical and role-aware routing is not operational. |
| Token/cost telemetry | **PARTIAL** | Provider usage, turn telemetry, and cost fields exist. Child usage is not consistently attributed or charged through `TaskGraph`. |
| Evaluation | **PARTIAL** | `arsy eval` supports arms, repeated trials, revision checks, token/safety parsing, Wilson intervals, and conservative baseline comparison. Trials run sequential commands in the same workspace; repository reset/isolation, competitor adapters, agent metrics, and full success rubrics remain. |

### TUI, CI, and distribution

| Capability | Status | Source-backed current behavior |
|---|---|---|
| TUI | **SHIPPED/PARTIAL** | Interactive turns, plan flow, approvals, provider/model controls, progress, and inspections exist. There is no durable multi-agent supervision view. |
| CI | **SHIPPED** | `.github/workflows/` is active: `ci.yml`, `fuzz.yml`, `performance.yml`, `cla.yml`, and the guardener pair run on GitHub Actions. Local checks are the same commands. |
| Release pipeline | **SHIPPED** | `release-please.yml` prepares the release; the tag push runs `release.yml`, which builds six Tier 1 archives, per-archive SHA-256, a Sigstore-signed `SHA256SUMS`, one CycloneDX SBOM per crate, and the installers, then pushes the Homebrew formula and Scoop manifest. `npm-publish.yml` follows it. |
| Installers and npm launcher | **SHIPPED/PARTIAL** | The installers are attached to each release and verify the archive digest, but not the Sigstore signature. The npm launcher downloads and verifies the matching archive at postinstall. |
| Update | **PRIMITIVE** | The CLI surface exists, but the current implementation reports the installed version as current without contacting a release source or installing anything. ARSY has no working self-update mechanism. |

## Competitive position

This comparison identifies pressure on the roadmap; it does not assert parity or
superiority. Competitor behavior changes and must be pinned by the benchmark
manifest used for any claim.

| Harness | Relevant observable strength | ARSY position at audited HEAD |
|---|---|---|
| Claude Code | Official documentation describes foreground/background subagents, permission surfacing, worktree isolation, steering, teams, hooks, plugins, and task inspection. Its proprietary core cannot be source-verified. | ARSY has stronger inspectable typed authority and events, but its child loop is synchronous and worktree coordination is not wired to agents. |
| OpenAI Codex / Codex CLI | Official documentation describes parallel subagents with inspectable threads and role/model configuration; current releases expose managed worktree and safety-review workflows. | ARSY has a provider-neutral operation boundary and durable graph primitives, but lacks a live agent lifecycle and independent safety review. |
| oh-my-openagent | Public project documentation emphasizes provider/model concurrency limits, background specialists, hooks, MCP, and LSP/AST workflows. | ARSY has narrower operational orchestration and a more explicit authority model. Claims beyond public behavior require pinned source inspection. |
| oh-my-pi | Public source and documentation expose parallel task fan-out, isolated workspaces, a hub for live jobs, LSP/AST breadth, DAP, and structured review. | ARSY has event, capability, policy, and proof primitives; it trails in agent lifecycle, supervision, workspace wiring, and semantic breadth. |

The competitive thesis is to make autonomous execution explainable and
independently verifiable. Feature parity matters where it supplies a missing
dependency for that thesis.

## Dependency order

```text
P0 Runtime truth and evidence durability
        │
        ├── Durable TaskGraph attempts, results, budgets, recovery
        │       ├── Async workers
        │       ├── messaging / wait / join / cancellation
        │       └── isolated writer wiring
        │               └── integration and conflict evidence
        │
        ├── acceptance criteria + revision-bound validation evidence
        │       └── completion proof + `arsy verify`
        │
        └── isolated evaluation trials and regression corpus

P1 Agent Hub ─────── consumes durable runtime projections
P1 Safe Auto ─────── consumes structured operations and evidence
P1 Role routing ──── consumes durable telemetry and eval observations

P2 semantic intelligence, memory curation, debugger repair loops
P3 remote workers, organization policy, distributed execution
```

Evidence durability starts with the runtime rather than waiting until the end.
Without it, an asynchronous task can finish but cannot prove which revision,
authority, attempt, or check produced its result. User-facing completion proofs
follow async execution and isolated writers because they must cover child
outputs and integration state.

## Roadmap overview

| Phase | Priority | Deliverable | Exit gate |
|---|---|---|---|
| 0 | P0 competitive core | Durable attempts, budget settlement, revision-bound validation | Crash/replay, lease fencing, and aggregate budget properties pass |
| 1 | P0 competitive core | Async scheduler, lifecycle, mailboxes, wait/join/cancel | Parent and three children progress concurrently and recover |
| 2 | P0 competitive core | Snapshot readers, worktree writers, typed integration | Two writers integrate without a shared mutable view |
| 3 | P0 competitive core | Acceptance criteria, completion proofs, `arsy verify` | Missing/stale evidence prevents verified status |
| 4 | P0 competitive core | Isolated eval corpus and regression gates | Comparable trials and safety gates are reproducible |
| 5 | P1 product experience | Agent Hub and supervision controls | Replayed UI supports inspect, steer, cancel, retry, diff, evidence |
| 6 | P1 product experience | Safe Auto and policy-backed roles | Auto and bypass differ; adversarial safety corpus passes |
| 7 | P1 product experience | Role-aware routing and plan compilation | Held-out routing gate beats eligible fixed baseline |
| 8 | P2 intelligence/ecosystem | Semantic expansion and automatic memory curation | Each addition proves held-out gain without trust regression |
| 9 | P1 operations / P3 scale | Launch readiness, then remote/platform maturity | Signed verified release first; distributed features remain ADR-gated |

## Plan → commitment → execution

Source inspection confirms three useful state classes:

```text
PLAN
process-local, cheap to revise, not a promise
        │ explicit approval / commit
        ▼
TODO
event-backed user-visible commitment
        │ compile only when dependencies or delegation require it
        ▼
TASK GRAPH
durable execution nodes, attempts, authority, budgets, results
```

`plan.*` should remain ephemeral by default. `plan.commit` is a future explicit
transition that reconciles TODOs rather than persisting every scratch mutation.
TaskGraph nodes are execution records, not a second checklist; cancellation or
completion flows back to TODO status through attributed events.

## Phase 0 — Runtime truth and evidence durability (P0)

### Goal

Make task, validation, budget, and recovery state truthful across restart before
introducing concurrency.

### Why now and existing primitives

`TaskGraph`, event storage, artifacts, TODO events, operation attribution,
leases, and `validate.record` supply most building blocks. Process-local
validation, incomplete budget accounting, and under-specified recovery would
become safety bugs once workers run independently.

### Missing pieces and architecture changes

- Add a durable task-attempt record containing task ID, parent task/attempt,
  role, assignee, model/profile decision, workspace requirement and lease,
  base revision, exact authority snapshot or derivation reference, budget
  reservation, input artifact, expected result schema, start time, lease, and
  terminal reason.
- Extend `TaskGraph`; do not add another scheduler store. Persist parent edges,
  attempt state, dependencies, reservations, usage settlement, result artifact,
  evidence references, and retry lineage as versioned task events.
- Serialize graph commands or revalidate transitions after optimistic-store
  contention. Current catch-up-and-retry behavior is not sufficient proof of a
  safe multi-worker command.
- Reserve child budget atomically before admission. Settle tokens, cost, tool
  calls, wall time, and attempts at termination; release unused capacity.
  Concurrent reservations must not exceed the delegating attempt's remainder.
- Persist validation records with source evidence artifact, command digest,
  task/attempt, workspace revision, operation, policy lineage, timestamp, and
  outcome. A later edit makes revision-bound evidence stale.
- Keep plans process-local and freely revisable by default. Add an explicit
  commit transition that produces/reconciles durable TODO commitments; compile
  commitments into TaskGraph nodes only when execution dependencies are needed.
- Paginate event replay. Separate conversation restoration from attempt
  recovery.
- On recovery, revalidate stored authority against current deny floors. A new
  policy may narrow or stop an attempt but never widen it. Fence late results
  with attempt ID and lease epoch.
- Separate execution `completed` from evidence `verified`.

### Persistence and API changes

Add versioned events conceptually equivalent to `task.attempt_started`,
`task.heartbeat`, `task.budget_reserved`, `task.usage_settled`,
`task.result_recorded`, `task.attempt_failed`, and `validation.recorded`.
Names are schema decisions, not necessarily model tool names. Persist schemas
before exposing asynchronous tools.

### Safety and testing strategy

Property tests cover authority attenuation, concurrent reservation, cycles,
idempotent terminal transitions, stale-lease fencing, replay, and policy
tightening during recovery. Crash tests stop after every transition and compare
the replayed projection with uninterrupted execution.

### Benchmark gate and definition of done

- Terminal state, parent, authority derivation, reserved/used budget, workspace
  revision, result, and evidence survive restart.
- Four simultaneous reservations cannot exceed the parent's remaining budget.
- An expired attempt cannot overwrite its retry.
- Validation remains queryable after restart and becomes stale after relevant
  revision change.
- Tests demonstrate the distinct lifetimes of Plan, TODO, and TaskGraph.

### Dependencies and explicit non-goals

Depends on the existing event/CAS and capability systems. Background workers,
distributed queues, scratch-plan event spam, and a new evidence database are
not part of this phase.

## Phase 1 — Async durable agent runtime (P0)

### Goal

Starting a child returns a durable task/attempt ID promptly while parent and
other ready nodes continue independently.

### Why now and existing primitives

The synchronous `Supervisor`, graph state machine, leases, dependencies,
observers, provider loop, and child grant construction are reusable. Replacing
`TaskGraph` would discard the strongest existing primitive.

### Architecture and runtime/API changes

- Put worker admission and lifecycle under a scheduler consuming the durable
  TaskGraph projection. It claims ready attempts with leases and bounded
  concurrency by provider, model, CPU, memory, workspace, and budget.
- Split today's inline `task.spawn` into start and observation semantics. The
  minimal model-facing surface provides start, list/status, send, wait, result,
  cancel, and retry/resume. `join` is a wait over existing
  `JoinPolicy::{All, Any, Quorum}`, not another scheduler concept. Retain
  blocking spawn only as a compatibility convenience.
- Define pending, blocked, ready, starting, running, cancelling, completed,
  failed, cancelled, abandoned, and superseded attempts. Derive task state from
  attempt/dependency state.
- Persist heartbeats only where leases represent liveness; avoid an event per
  tick. A renewed lease/epoch record and monotonic last-activity projection are
  sufficient for local workers.
- Use bounded retries with explicit retryability, backoff, budget, and new
  attempt IDs. Never retry denials, invalid schemas, or destructive operations
  whose outcome is unknown.
- Wake dependencies from committed terminal events and define failed/cancelled
  dependency behavior while preserving partial evidence.
- Validate typed result artifacts against `required_output`; a human summary
  cannot replace the structured result.
- Persist a child transcript or loss-bounded child event projection for result,
  usage, decisions, and failures.
- Propagate cancellation to model streams and cancellable operations. File
  cancellation occurs at safe boundaries; process cancellation records cleanup.

### Messaging and coordination

Each attempt receives a durable mailbox. Messages contain sender, recipient,
task/attempt, causal event, kind, schema version, body artifact, and delivery
state. Initial kinds are instruction update, question, answer, progress,
partial result, reviewer feedback, cancellation reason, and dependency unblock.
Parent-child messaging ships first. Peer messages require an explicit graph
relationship or supervisor grant. Steering applies at the next safe model
boundary and remains auditable.

### User-visible capabilities

- A parent starts several investigations, continues using tools, inspects
  status, sends updates, and waits for selected results.
- `arsy resume` reconstructs attempts and resumes them safely or explains why
  re-admission is required.
- Results name producing attempt, revision, cost, and evidence.

### Safety, testing, and benchmark gate

Test duplicate claims, late completion, cancellation during model/tool/process
activity, provider failure, dependency failure, mailbox order/backpressure,
restart, and authority narrowing.

Given three independent repository investigations, ARSY must execute them
concurrently, let the parent perform operations during execution, and restore
every attempt after a forced restart. Cancellation leaves an auditable terminal
state and all artifacts. Child usage is charged and no child exceeds parent
authority or budget.

### Dependencies and explicit non-goals

Depends on Phase 0. Remote workers, unrestricted peer chat, speculative
execution, and shared-workspace parallel writers are deferred.

## Phase 2 — Isolated writer agents and integration (P0)

### Goal

Allow parallel implementers without sharing a mutable filesystem view.

### Existing primitives and missing pieces

ADR-0010, `WorkspaceRequirement::IsolatedWriter`, and
`WorkspaceCoordinator::{writer, reader, recover_expired, merge_git}` exist.
They are test-only, use in-memory ownership, create detached worktrees, and do
not provide a complete patch/integration lifecycle.

### Architecture and runtime changes

- Make workspace assignment part of attempt admission. Bind each attempt to
  repository identity, base revision, view path, backend, mutability, owner,
  lease epoch, and expiry before granting file/process tools.
- Route read-only agents through a revision-bound coordinator view. A child with
  `process.exec` is read-oriented, not read-only, unless the sandbox makes the
  view immutable and limits external effects.
- Route `IsolatedWriter` runtime roots to their worktree/copied snapshot. Grant
  write authority only for that view, never the main workspace.
- Use deterministic, collision-safe internal branch/ref names keyed by session,
  task, attempt, and agent. Record refs and paths without treating names as
  authority.
- Require a clean, recorded base or an explicit decision for dirty main
  workspaces. Capture dirty state as an artifact or refuse admission; never
  silently omit it.
- Define writer output as base revision, head revision or patch artifact,
  changed-file manifest, validation evidence, and unresolved state.
- Implement integration as a typed, policy-checked operation. Preflight stale
  base, overlap, validations, cleanliness, and target revision. Record conflict
  evidence and abort partial Git merges.
- Serialize shared Git ref/worktree administration while allowing filesystem
  work in distinct views concurrently.
- Persist worktree leases and recovery actions. Preserve crashed writers with
  commits/evidence for inspection; clean abandoned empty views through an
  explicit retention policy.
- Give an integrator role exclusive target-view authority. A writer cannot
  self-integrate unless policy explicitly grants both responsibilities and the
  proof gate passes.

### Safety and testing strategy

Test confinement, symlinks, Git lock contention, dirty/stale bases, conflicting
and independent patches, crash after commit/during integration, cleanup,
non-Git copies, and Windows paths. Compile writer process sandboxes against the
isolated root.

### Benchmark gate and definition of done

Two writers modify independent components concurrently without sharing a
mutable tree. An integrator applies both to a recorded target after validation.
A conflict never leaves the target partially merged. Restart discovers every
owned or abandoned view and retains evidence.

### Dependencies and explicit non-goals

Depends on durable attempts and async workers. Overlay/COW backends, remote
workspaces, and automatic semantic conflict resolution remain deferred until
Git/copy measurements justify them.

## Phase 3 — Proof-carrying completion (P0)

### Goal

Bind completion to explicit acceptance criteria and independently checkable
evidence.

### Existing primitives and completion model

Operation artifacts, event causation, TODOs, TaskGraph results, process results,
diagnostics, review, and `validate.record` feed the proof. A second evidence
store is unnecessary. Today provider success completes a turn/task without a
criteria or validation gate.

```text
Task / committed TODO
├── acceptance criterion (stable ID, verifier, required/optional)
│   ├── implementation artifact or revision
│   ├── validation evidence bound to that revision
│   └── diagnostics/review evidence
├── unresolved failure or explicit unverifiable reason
└── completion proof
    ├── task + attempt + integrated revision
    ├── criterion verdicts and evidence references
    ├── policy denials/violations and approvals
    ├── stale or superseded evidence
    └── proof schema/version and verification result
```

### Architecture and runtime/API changes

- Add stable typed acceptance criteria to committed work. Each names verifier
  class, applicability, freshness, and whether human judgment is unavoidable.
- Compile executable criteria into validation nodes/dependencies. Models cannot
  assert their own test results; use operation artifacts, diagnostics,
  revisions, and independent reviewer outputs.
- Store a completion-proof manifest as an artifact referencing canonical
  evidence/events. Keep content-addressed artifacts as the evidence store.
- Add proof states: unverified, partially verified, verified, failed, stale,
  and unverifiable. Execution completed must never render as verified.
- Add `arsy verify <session-or-task>` as a read-only verifier that rebuilds the
  proof, checks schema/digest/revision, and exits nonzero for missing, failed,
  stale, or contradictory required evidence. Reruns require separate authority.
- Carry proof references through integration. Writer-revision evidence must be
  rerun or declared applicable when integrated content differs.
- Represent human acceptance and reviewer judgment as attributed evidence,
  never deterministic test results.

### Safety and testing strategy

Test forged artifact IDs, evidence for the wrong command, another revision/task,
superseded attempts, missing artifacts, policy violations, optional/human
criteria, and post-validation edits.

### Benchmark gate and definition of done

ARSY cannot mark a task verified while any required criterion lacks valid,
fresh evidence. `arsy verify` reaches the same verdict after restart and in CI.
Every verdict links task, revision, operation, actor, and artifact.

### Dependencies and explicit non-goals

The event schema starts in Phase 0; complete proof generation follows isolated
integration. No theorem prover, model-confidence-as-proof, or duplicate blob
store is planned.

## Phase 4 — Competitive benchmark and regression gates (P0)

### Goal

Measure harness quality under reproducible starting conditions and make safety
regressions release-blocking.

### Required work

- Extend the current eval suite. Run every trial in a fresh checkout/snapshot
  pinned to repository revision and environment image.
- Define manifests containing prompt, starting revision, hidden tests,
  acceptance criteria, allowed capabilities, approval mode, sandbox assurance,
  model/provider, timeout, seedable settings, and expected artifacts.
- Add competitor adapters only where licensing and automation permit. Pin exact
  harness versions and preserve observable configuration. Label non-equivalent
  permissions, prompts, and hidden behavior.
- Capture success, hidden-test pass rate, regressions, wall time, input/output/
  cached tokens, cost, approvals, denials, violations, tool failures, retries,
  changed files, compactions, agents, parallelism, integration conflicts, and
  proof completeness.
- Use paired tasks, repeated trials, held-out suites, confidence intervals, and
  cost-normalized reports. Preserve raw events and environment manifests.
- Cover exploration, fixes, diagnosis, conflicts, long context, unsafe requests,
  restart, parallel readers/writers, and evidence fraud.

### CI strategy

```text
pull request: Linux fmt, clippy, focused tests, compatibility fixtures
main:         Linux full workspace suite + deterministic eval smoke
scheduled:    Linux/macOS/Windows sandbox, fuzz, performance, held-out evals
release:      full target matrix, installers, SBOM, checksums, signatures
```

Use path filters and cancel superseded runs. Restore workflows individually
after updating their pinned Rust toolchain and validating monthly cost. Release
workflows are a separate launch gate; they do not justify every-OS PR CI.

### Benchmark gate and definition of done

The same repository, revision, task, model, provider, timeout, permissions, and
environment produce isolated comparable trials where possible. Reports quantify
every deviation. “ARSY is better” requires a preregistered metric, sufficient
trials for the stated uncertainty, no safety regression, and a durable report.

### Dependencies and explicit non-goals

Trial isolation can proceed beside Phases 1–3; multi-agent comparisons depend on
them. Public leaderboards, synthetic scores without hidden tests, and one-run
claims are excluded.

## Phase 5 — Agent Hub and supervision (P1)

### Goal

Expose durable orchestration state and controls through the TUI and service
protocol.

### Architecture and user-visible capabilities

Build the Hub as a projection of events, TaskGraph, leases, telemetry, and
proofs. One row shows agent, task/parent, role, attempt/state, current operation
class, elapsed/last activity, tokens/cost/budget, workspace/worktree, changed
files, validation/proof state, capability summary, and result.

Controls are inspect transcript, inspect policy/authority, message/steer, pause
admission, resume/retry, cancel, open diff/evidence, and integrate. Every control
maps to a typed service command, policy decision where needed, and event. Pause
initially acts at safe model/tool boundaries. Accessibility requires keyboard
operation, screen-reader labels, non-color status, and bounded update frequency.

### Testing, gate, dependencies, and non-goals

Projection replay renders the same state after restart; race tests cover finish
while inspected/cancelled. With four agents, an operator can identify blocked
work, steer/cancel, inspect diff/evidence, and resume failure without losing
attribution. The Hub depends on Phases 1–3 and is neither canonical state nor a
free-form team-chat product.

## Phase 6 — Safe Auto and reviewer roles (P1)

### Goal

Make `auto` autonomous subject to independent safety review while preserving
`bypassPermissions` as an explicit high-risk approval behavior inside hard
policy ceilings.

```text
typed operation + user intent + resolved resources
        │
        ├── deterministic policy and hard denies
        ├── deterministic risk rules
        └── bounded model reviewer when rules are insufficient
                    │
             allow | require approval | deny
```

The reviewer receives structured user intent, operation kind, capabilities,
canonical targets/network destinations, reversibility, workspace cleanliness,
sandbox assurance, credential involvement, environment/repository trust, and
destructive potential. Arbitrary tool/repository output is omitted unless a
defined redacted field is required.

### Required work

- Review after preparation/resource resolution and hard policy evaluation,
  before approval-mode conversion and dispatch. Review can narrow allow to
  approval/deny; it cannot grant capability or override deny.
- Deterministically classify safe reads, scope escapes, forbidden network,
  credentials, destructive/irreversible effects, dirty workspaces, and missing
  sandbox assurance. Use a model only for residual intent ambiguity.
- Version reviewer schema, prompt, model/profile, timeout, budget, and reasons.
- Cache exact decisions only, keyed by intent, operation digest, resource and
  policy revisions, trust state, reviewer version, and expiry.
- Fail closed for expansion, credentials, system modification, destructive or
  irreversible effects, unknown sandbox, and policy uncertainty. Reviewer
  outage may fall back to human approval only for low-risk reversible work;
  unattended mode denies.
- Emit review request/result/failure with redacted inputs, cost, latency,
  decision, and policy relationship.
- Define responsibility-based roles: explorer, planner, implementer, debugger,
  test runner, reviewer, integrator, and safety reviewer. Each role declares
  capabilities, workspace, model needs, budget, result schema, delegation, and
  write rights. Prompts present these contracts; runtime policy enforces them.

### Gate, dependencies, and non-goals

Safe Auto executes ordinary bounded coding flows and approves/refuses
destructive, credential-bearing, out-of-scope, untrusted-network, or weakly
sandboxed actions. Adversarial repository instructions cannot widen authority.
`auto` and `bypassPermissions` have distinct tested outcomes while explicit
denies remain final. This depends on structured attempts/evidence and benchmark
safety cases. Natural-language-only decisions and fail-open high-risk behavior
are excluded.

## Phase 7 — Role-aware model routing and plan compilation (P1)

### Goal

Select provider-neutral models from policy-eligible candidates using role,
requirements, observed performance, latency, and cost.

### Required work

- Extend existing routing/model profiles with role requirements: context,
  structured/tool output, reasoning, modalities, latency class, residency, and
  provider features.
- Persist probes and observations with time, provider/model version, samples,
  eval suite, role/task class, cost, latency, and outcome.
- Feed real agent telemetry into rolling observations; use held-out evals for
  quality rather than self-declared success.
- Keep allowlists, residency, cost ceilings, and capabilities as eligibility
  filters before ranking. Record selection reasons and rejected candidates.
- Define plan approval as a commitment boundary. A committed plan reconciles
  TODOs and compiles dependency-bearing work into TaskGraph nodes; scratch edits
  never create agents implicitly.

### Gate, dependencies, and non-goals

Routing is reproducible from a dated observation set and cannot overcome a hard
allowlist. It beats an eligible fixed baseline on held-out role-stratified tasks
without safety regression and reports confidence/cost. Missing data yields an
explained fallback. This depends on roles, telemetry, and isolated evals. No
provider-specific role mapping or self-modifying production prompt is allowed.

## Phase 8 — Intelligence and memory expansion (P2)

### Goal

Improve evidence quality where semantics can prevent broad edits or unnecessary
agents, after orchestration is dependable.

### Code intelligence

- Add language adapters through existing LSP servers and tree-sitter grammars,
  prioritized by benchmark failures.
- Enrich the incremental map/graph with revision-bound references/callers,
  symbols, tests, generated-code markers, diagnostics, and provenance.
- Add structural search, impact analysis, semantic patch planning, and automatic
  post-edit diagnostics as typed operations.
- Extend DAP toward policy-limited sessions only where repair benchmarks show
  value; keep launch, observations, patch, and validation as evidence.

### Memory automation

```text
observation → candidate → trust/provenance vetting → confidence + expiry
            → event-backed MemoryStore → budgeted, cited retrieval
```

- Generate candidates from attributed observations/proof artifacts.
- Preserve origin ceilings: repository/model content cannot become operator or
  user truth. Surface conflicts and supersede only under policy.
- Apply the existing scope, confidence, expiry, supersession, revocation,
  secret-vetting, and provenance model.
- Retrieve exact scoped facts first, then graph/semantic candidates, under a
  measured budget. Every context insertion cites its record/provenance.
- Evaluate precision, stale-memory harm, conflicts, retrieval gain, token cost,
  and prompt-injection resistance before default automation.

### Gate, dependencies, and non-goals

Each semantic addition improves a held-out task class over the current
LSP/graph/text cascade. Automatic memory improves verified success or token use
without elevating untrusted authority or stale-memory regressions. This depends
on proof-aware evals. Bespoke language engines, embedding-only retrieval, global
memory, and silent high-trust memory are excluded.

## Phase 9 — Launch readiness and platform maturity

Launch readiness is **P1 operational work**. Before a channel is called
operational:

- restore and validate the relevant workflow files;
- update release Rust 1.85 to the workspace's 1.98 or derive one source;
- build/test the target matrix and sandbox conformance;
- attach the installers referenced by release URLs;
- align SBOM documentation with CycloneDX or intentionally add SPDX;
- create a canonical checksum manifest from final bytes;
- implement Sigstore signing/bundles and verify workflow identity cleanly;
- publish platform npm packages before the launcher and clean-install each;
- wire Homebrew/Scoop only to the same immutable digests;
- record a real release smoke test.

The update command remains non-operational until it checks a trusted signed
manifest. Automatic installation remains deferred because it adds persistent
network/write authority; package-manager and explicit installer updates are
sufficient for launch.

Remote workers, daemon scheduling, organization policy, distributed leases,
team collaboration, and a marketplace are **P3**. Each needs a new accepted ADR
covering trust, identity, transport, secrets, replay, clocks/leases, compatibility,
and recovery. None is on the current competitive critical path.

## Multi-agent role contracts

Roles are policy/runtime presets, not provider-specific prompt characters.

| Role | Default authority/workspace | Expected result | Key constraint |
|---|---|---|---|
| Explorer | read/search; revision-bound snapshot | findings with source/evidence refs | command use separately granted |
| Planner | read + plan operations | criteria, TODO reconciliation, graph draft | cannot commit/execute without transition authority |
| Implementer | scoped write/process; isolated writer | revision/patch, changed files, validations | cannot mutate main view |
| Debugger | read/process/debug under sandbox | reproducible diagnosis and debug evidence | attach/evaluate/memory actions separately gated |
| Test runner | read/process on candidate revision | typed validation record | cannot edit result or implementation |
| Reviewer | read candidate/base/evidence | typed findings and criterion verdicts | no self-approval or integration |
| Integrator | Git write on target + proof read | integration revision/conflict evidence | one target writer; proof gate required |
| Safety reviewer | no execution capabilities | allow/approval/deny recommendation | may only narrow policy outcome |

Each contract also defines model requirements, default/max budget, delegation
depth, result schema, retries, and evidence. Operators may customize presets
within policy ceilings.

## Roadmap invariants

1. No child receives authority the parent attempt did not hold and record.
2. Policy or safety review may narrow delegated authority; neither may widen it.
3. Parallel writers never share the same mutable filesystem view.
4. A mutable view has one writer owner and a recorded base revision.
5. Task and attempt terminal state survives restart.
6. Late or duplicate workers cannot commit after their lease epoch ends.
7. Parent and child reservations cannot exceed the parent's remaining budget.
8. Messages, steering, cancellation, retries, and feedback are attributable.
9. Cancellation preserves evidence and records cleanup/unknown-effect state.
10. Tool batching cannot bypass contracts, policy, hooks, or grant checks.
11. Background process sessions are never represented as background agents.
12. Plan Mode cannot delegate an effect it would refuse itself.
13. Scratch plans, committed TODOs, and TaskGraph nodes remain distinct.
14. “Verified” requires fresh evidence for every required criterion.
15. A successful model turn or process exit alone is not verification.
16. Evidence binds operation, task/attempt, actor, artifact, and revision;
    stale evidence remains visible.
17. Safety reviewers have no execution authority and cannot override hard deny.
18. `auto` must differ from `bypassPermissions` before Safe Auto is shipped.
19. Routing cannot bypass provider/model/residency/cost allowlists.
20. Repository, plugin, hook, MCP, model, LSP, and DAP content cannot increase
    authority or silently create high-trust memory.
21. The Agent Hub is a replayable projection, never canonical state.
22. Provider adapters normalize model events and never mutate workspaces.
23. Compatibility formats never become core task, role, proof, or capability
    types.
24. Competitive claims require pinned reproducible benchmark evidence.

## Highest-priority implementation slice

Implement Phase 0's durable task-attempt envelope and budget/evidence events as
the first P0 milestone. Then refactor synchronous child execution behind a
scheduler-owned worker that returns an attempt ID. Async execution without
durable authority, budget settlement, revision evidence, and lease fencing would
make current recovery gaps unsafe and harder to diagnose.

1. Extend task events with parent identity and a versioned attempt projection.
2. Add atomic budget reserve/settle and attempt lease epochs.
3. Persist typed result and validation evidence references with revision.
4. Replay and crash-test every transition.
5. Change `task.spawn` to admit an attempt and return its ID; run the current
   child loop in a bounded local worker.
6. Add status/result/wait/cancel against that projection.

## Open architectural questions

- Whether the first local scheduler lives in the CLI process or behind the
  existing service protocol. The persisted contract must permit either;
  daemonization is not needed for the first async gate.
- Whether heartbeats append events or update a rebuildable lease projection.
- How dirty main-workspace state becomes a writer base: captured patch,
  temporary commit, or refusal. Silent omission is forbidden.
- Which writer validation remains applicable after integration and which checks
  must rerun on the target revision.
- Whether pause remains cooperative at model/tool boundaries or later needs
  platform process suspension.
- Which Safe Auto deterministic classes are portable across OS assurances.
- Minimum samples/effect size for routing and public competitive claims.
- Which task failures justify the next grammar, LSP adapter, or DAP session.

## Explicitly deferred

Remote/distributed workers, overlay/COW workspace backends, unrestricted peer
chat, automatic semantic conflict resolution, universal persistent debugger
sessions, global memory, public marketplace, organization collaboration,
self-update, and all-platform per-PR CI are deferred until their local runtime,
policy, benchmark, or release dependencies are met.
