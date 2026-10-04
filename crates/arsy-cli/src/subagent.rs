//! Subagents: children of a task, holding less authority than it does.
//!
//! # Why a child is not just another prompt
//!
//! "Spawn a subagent" usually means sending the model a second conversation
//! and pasting the answer back. That is a prompt wrapper: the child can do
//! everything the parent could, nothing recorded it, and an interruption loses
//! it. Here a child is a node in the same durable task graph, with its own
//! lease, its own budget taken out of the parent's, and its own capability
//! grants attenuated from the parent's — narrower scope, shorter expiry, one
//! less delegation left.
//!
//! # Writer isolation, and why it is a consequence rather than a feature
//!
//! A child's grants become the rule set its tool runtime evaluates against. A
//! child that asked to read and search therefore *cannot* write, whatever
//! policy would have allowed the parent — not because a flag says
//! `ReadOnlySnapshot`, but because no rule in its runtime permits `fs.write`.
//! One writer per workspace falls out of that: the parent is the only task
//! that ever asked for write authority.
//!
//! # What the observer is for
//!
//! The supervisor watches each child through redacted projections of what the
//! child's runtime did — tool names and outcomes, never arguments or file
//! contents. It may suggest, and, when its authority allows, deny: a child
//! that spends its round failing is stopped rather than left to burn the
//! budget it was given. Every intervention is recorded with what it cost.

use crate::{provider, Emitter};
use arsy_code::{
    agent::{ExecutionMode, ToolResult, ToolRuntime},
    workspace::{
        DirtyPolicy, IntegrationRequest, MergeOutcome, ViewOwner, WorkspaceCoordinator,
        WorkspaceLease,
    },
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant, ResourcePattern, ResourceScope},
    config::Config,
    domain::{AgentId, AssignmentId, AttemptId, CriterionId, Principal, SubscriptionId, TaskId},
    observer::{Intervention, ObserverAuthority, ObserverSubscription, RedactedProjection},
    orchestration::{
        AcceptanceCriterion, AttemptOutcome, AttemptRequest, AttemptState, Budget,
        ChildCapabilityRequest, DirtyDisposition, IsolationBackend, JoinPolicy, MessageKind,
        Retryability, TaskGraph, TaskNode, TaskState, Verifier, WorkspaceAssignment,
        WorkspaceRequirement, WriterResult,
    },
    policy::{ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance},
    proof::{Prover, PROOF_MEDIA_TYPE},
    protocol::IdempotencyKey,
    provider::{
        CanonicalModelRequest, ModelContent, ModelKey, ModelMessage, ModelRole, ToolSchema,
    },
    safety::AgentRole,
    scheduler::{Admission, CancelToken, Scheduler, SchedulerError},
    validation::ValidationLog,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The most children one turn may spawn.
///
/// A model that can spawn without bound spawns without bound. Low enough that
/// a runaway is a nuisance rather than a bill.
pub const MAX_CHILDREN: usize = 4;

/// The share of the parent's remaining budget one child may take.
///
/// A quarter, so four children fit and the parent keeps something to read
/// their answers with.
const CHILD_BUDGET_SHARE: u64 = 4;

/// What a child may ask for. Read-only by name: writing is the parent's job,
/// and a list is what makes that reviewable rather than implied.
const DELEGABLE: &[(&str, CapabilityAction)] = &[
    ("fs.read", CapabilityAction::FsRead),
    ("process.exec", CapabilityAction::ProcessExec),
    // Only into an isolated view, and only when policy delegates it. The
    // check that enforces that is in `start`, next to the workspace choice
    // it depends on.
    ("fs.write", CapabilityAction::FsWrite),
];

/// How many failures in a row mean a child is not working.
///
/// Three: one is ordinary, two can be a correction, and a third in a row is a
/// child repeating itself with the rounds someone else is paying for.
const CONSECUTIVE_FAILURES: u64 = 3;

/// What an observer may spend on one child, in micro-units of its own budget.
/// Each intervention costs one; the bound is how many times it may act before
/// it has to stop watching.
const OBSERVER_BUDGET: u64 = 16;

/// How many children one turn may still start.
///
/// A type rather than a counter beside a check, because the two have to move
/// together: the bug this replaces was a check with no increment anywhere, and
/// nothing about a bare `usize` made that visible.
#[derive(Debug, Default)]
struct Allowance {
    started: usize,
}

impl Allowance {
    /// Spend a slot, or say why there is none. Counts the attempt, not the
    /// outcome.
    fn take(&mut self) -> Result<(), String> {
        if self.started >= MAX_CHILDREN {
            return Err(format!(
                "this turn has already spawned {MAX_CHILDREN} subagents; do the rest yourself"
            ));
        }
        self.started += 1;
        Ok(())
    }
}

/// The tool a supervisor offers on top of the workspace tools.
///
/// Not a workspace operation: spawning changes no file and runs no command, it
/// adds a node to the session's own graph. The child's effects each go through
/// the one path a tool call takes, under the child's own grants.
pub fn schemas(delegates: bool) -> Vec<ToolSchema> {
    let object = |properties: Value, required: Value| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };
    // Offered whether or not this workspace delegates anything: committing to
    // what "done" means is not delegation, and a turn that cannot spawn still
    // has to be able to say what it is to be held to.
    let mut schemas = vec![ToolSchema {
        name: "task.criterion".to_owned(),
        description:
            "Commit one acceptance criterion for this task, before doing the work. State it, and \
             say what decides it: `check` names the exact command whose recorded pass settles it, \
             `review` means a reviewer must say so, `human` means only a person can. A criterion \
             with a `check` is met only by a recorded run of that exact command, against this \
             revision — saying it passed is not evidence."
                .to_owned(),
        input_schema: object(
            json!({
                "statement": {"type": "string", "description": "What has to hold, stated so it can be checked."},
                "check": {"type": "string", "description": "The exact command that settles it."},
                "decided_by": {
                    "type": "string",
                    "enum": ["check", "review", "human", "unverifiable"],
                    "description": "Defaults to `check` when `check` is given, `review` otherwise."
                },
                "required": {"type": "boolean", "description": "Defaults to true."},
                "why_unverifiable": {"type": "string", "description": "Required when `decided_by` is `unverifiable`."}
            }),
            json!(["statement"]),
        ),
    }];
    if !delegates {
        return schemas;
    }
    schemas.extend([
        ToolSchema {
            name: "task.spawn".to_owned(),
            description: format!(
                "Start a subagent that can read and search but cannot write, and return its ids \
                 straight away without waiting for its answer. Start every independent \
                 investigation you have before reading any of them back, then use `task.wait` or \
                 `task.result`. At most {MAX_CHILDREN} per turn."
            ),
            input_schema: object(
                json!({
                    "goal": {
                        "type": "string",
                        "description": "What the subagent should find out, stated so its answer is useful on its own."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["explorer", "planner", "implementer", "debugger", "test_runner", "reviewer", "integrator", "safety_reviewer"],
                        "description": "Responsibility contract for this child. Defaults to `explorer`."
                    },
                    "capabilities": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["fs.read", "process.exec", "fs.write"]},
                        "description": "What it may do. Defaults to `fs.read`. `fs.write` needs an isolated writer."
                    },
                    "workspace": {
                        "type": "string",
                        "enum": ["read_only_snapshot", "isolated_writer"],
                        "description": "`read_only_snapshot` (default) pins it to this revision and cannot change anything. `isolated_writer` gives it a tree of its own to change; apply its work with `task.integrate`."
                    }
                }),
                json!(["goal"]),
            ),
        },
        ToolSchema {
            name: "task.integrate".to_owned(),
            description:
                "Apply one finished writer's work to this workspace. Refuses a writer that ran no \
                 checks, left something unresolved, started from a revision this workspace has \
                 left, or touches a file an earlier integration in this turn already changed. A \
                 conflict leaves the workspace unchanged."
                    .to_owned(),
            input_schema: object(
                json!({
                    "task": {"type": "string", "description": "A writer's task id from `task.spawn`."},
                    "expect_target_revision": {
                        "type": "string",
                        "description": "The revision this workspace was on when you decided to apply it."
                    }
                }),
                json!(["task"]),
            ),
        },
        ToolSchema {
            name: "task.status".to_owned(),
            description:
                "Report every subagent this turn started, with its state and whether its answer \
                 is ready. Takes no arguments."
                    .to_owned(),
            input_schema: object(json!({}), json!([])),
        },
        ToolSchema {
            name: "task.wait".to_owned(),
            description:
                "Wait for subagents to finish and return their answers. Waits for all of them by \
                 default; `join` may be \"any\" to return as soon as one answers, or a number for \
                 that many."
                    .to_owned(),
            input_schema: object(
                json!({
                    "tasks": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Task ids from `task.spawn`. Defaults to every subagent this turn started."
                    },
                    "join": {
                        "type": "string",
                        "description": "\"all\" (default), \"any\", or a count such as \"2\"."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait before reporting what is still running."
                    }
                }),
                json!([]),
            ),
        },
        ToolSchema {
            name: "task.result".to_owned(),
            description:
                "Read what one subagent produced. Says so rather than waiting when it has not \
                 finished."
                    .to_owned(),
            input_schema: object(
                json!({"task": {"type": "string", "description": "A task id from `task.spawn`."}}),
                json!(["task"]),
            ),
        },
        ToolSchema {
            name: "task.cancel".to_owned(),
            description:
                "Stop a subagent whose answer you no longer need. Whatever it already established \
                 is kept."
                    .to_owned(),
            input_schema: object(
                json!({
                    "task": {"type": "string", "description": "A task id from `task.spawn`."},
                    "reason": {"type": "string", "description": "Why it is no longer needed."}
                }),
                json!(["task"]),
            ),
        },
        ToolSchema {
            name: "task.send".to_owned(),
            description:
                "Send a running subagent a narrowing instruction or an answer to its question. It \
                 reaches the subagent between its rounds and cannot give it any new permission."
                    .to_owned(),
            input_schema: object(
                json!({
                    "task": {"type": "string", "description": "A task id from `task.spawn`."},
                    "message": {"type": "string", "description": "What to tell it."}
                }),
                json!(["task", "message"]),
            ),
        },
    ]);
    schemas
}

/// The calls a supervisor answers rather than the workspace runtime.
pub const OWNED_TOOLS: &[&str] = &[
    "task.criterion",
    "task.spawn",
    "task.status",
    "task.wait",
    "task.result",
    "task.cancel",
    "task.send",
    "task.integrate",
];

/// One turn's authority to create and run children.
pub struct Supervisor<'a> {
    root: PathBuf,
    config: &'a Config,
    resolved: &'a provider::Resolved,
    model: String,
    parent: TaskId,
    /// What the parent may hand on. Empty when policy grants nothing that may
    /// be delegated, which is the default and refuses every spawn.
    delegable: Vec<CapabilityGrant>,
    /// The parent's execution ceiling. A child runs under its own runtime, so
    /// without this a supervisor in Plan Mode could delegate the write it is
    /// itself refused — authority is attenuated by delegation, never widened.
    mode: ExecutionMode,
    allowance: Allowance,
    /// What the observers did, for the turn's record.
    interventions: Vec<Value>,
    /// Built on the first spawn, over its own view of the session's stream.
    /// A turn that never delegates pays for nothing.
    scheduler: Option<Scheduler>,
    children: Vec<Child>,
    /// Cuts and cleans up the views children run in.
    coordinator: WorkspaceCoordinator,
    /// Paths already applied to the target in this turn, so a second writer
    /// cannot quietly overwrite the first one's file.
    integrated: Vec<String>,
}

/// One child this turn started, and the thread running it.
struct Child {
    task: TaskId,
    attempt: AttemptId,
    goal: String,
    role: AgentRole,
    cancel: CancelToken,
    /// Taken when the child is reaped. `None` afterwards.
    worker: Option<JoinHandle<Report>>,
    /// What the worker returned, once it has been reaped.
    report: Option<Report>,
    /// The view it runs in, kept so it can be cleaned up.
    lease: Option<WorkspaceLease>,
    assignment: AssignmentId,
    writer: bool,
}

/// What a finished child hands back to the turn that started it.
struct Report {
    answer: Result<String, String>,
    interventions: Vec<Value>,
}

impl<'a> Supervisor<'a> {
    pub fn new(
        root: PathBuf,
        config: &'a Config,
        resolved: &'a provider::Resolved,
        model: String,
        parent: TaskId,
        runtime: &ToolRuntime,
    ) -> Self {
        let delegable = runtime.delegable_grants(
            &DELEGABLE
                .iter()
                .map(|(_, action)| *action)
                .collect::<Vec<_>>(),
        );
        Self {
            root,
            config,
            resolved,
            model,
            parent,
            delegable,
            mode: runtime.execution_mode(),
            allowance: Allowance::default(),
            interventions: Vec::new(),
            scheduler: None,
            children: Vec::new(),
            coordinator: WorkspaceCoordinator::default(),
            integrated: Vec::new(),
        }
    }

    /// Whether this turn should be offered the spawn tool at all.
    ///
    /// A workspace whose policy delegates nothing gets no spawn tool rather
    /// than a tool that always refuses: an offered tool the model cannot use
    /// costs a round to discover.
    pub fn can_delegate(&self) -> bool {
        !self.delegable.is_empty()
    }

    pub fn interventions(&self) -> &[Value] {
        &self.interventions
    }

    /// Answer one of the calls in [`OWNED_TOOLS`].
    pub fn call(
        &mut self,
        name: &str,
        arguments: &Value,
        graph: &mut TaskGraph,
        emitter: &mut Emitter,
    ) -> ToolResult {
        let started = Instant::now();
        let outcome = match name {
            "task.criterion" => self.declare(arguments, graph),
            "task.spawn" => self.start(arguments, graph),
            "task.status" => self.status(),
            "task.wait" => self.wait(arguments, emitter),
            "task.result" => self.result(arguments, emitter),
            "task.cancel" => self.stop(arguments),
            "task.send" => self.steer(arguments),
            "task.integrate" => self.integrate(arguments),
            other => Err(format!("{other} is not a supervisor call")),
        };
        match outcome {
            Ok(output) => ToolResult {
                tool: name.to_owned(),
                success: true,
                output,
                changed_files: Vec::new(),
                duration: started.elapsed(),
                metadata: Value::Null,
                artifact: None,
            },
            Err(reason) => ToolResult::refused(name, reason),
        }
    }

    /// Stop and reap every child before the turn ends.
    ///
    /// A turn that returned while its children were still reading would leave
    /// threads spending a budget nobody is waiting on, and attempts whose only
    /// route to a terminal state is their lease running out. So the turn's end
    /// is where they stop, and what they had is recorded.
    pub fn finish(&mut self, emitter: &mut Emitter) {
        for index in 0..self.children.len() {
            if self.children[index].worker.is_none() {
                continue;
            }
            let (task, attempt) = (self.children[index].task, self.children[index].attempt);
            if let Some(scheduler) = self.scheduler.as_mut() {
                let _ = scheduler.cancel(attempt, "the turn that started it ended");
            }
            self.children[index].cancel.cancel();
            self.reap(index, emitter);
            emitter.trace("subagent.reaped", json!({"task": task.to_string()}));
        }
        self.release_views(emitter);
    }

    /// Build this task's completion proof, store it, and promote the task
    /// only if it holds.
    ///
    /// Called at the end of a turn, from outside the model's reach. A turn
    /// that ran cleanly and met nothing it committed to ends `Completed`,
    /// which is the whole distinction: execution finished is not verified.
    pub fn settle_proof(&self, graph: &mut TaskGraph, emitter: &mut Emitter) {
        if graph.criteria_of(self.parent).is_empty() {
            return;
        }
        let Ok(validations) =
            ValidationLog::open(graph.store(), graph.session(), Principal::System)
        else {
            return;
        };
        let artifacts = arsy_kernel::artifact::FileArtifactStore::open(
            self.root.join(arsy_code::workspace::ARTIFACTS),
            0,
        )
        .ok();
        let proof = Prover::new(
            graph,
            validations.records(),
            artifacts
                .as_ref()
                .map(|store| store as &dyn arsy_kernel::artifact::ArtifactStore),
        )
        .prove(
            self.parent,
            arsy_code::git::revision(&self.root),
            arsy_kernel::artifact::unix_time_ms(),
        );

        // Stored so a later reader has something to compare a rebuild
        // against. The verdict never comes from the stored copy.
        let manifest = serde_json::to_vec(&proof).unwrap_or_default();
        if let Some(store) = artifacts.as_ref() {
            let _ = store.put(
                &manifest,
                arsy_kernel::artifact::NewArtifact {
                    media_type: PROOF_MEDIA_TYPE.to_owned(),
                    creator: Principal::System,
                    source_revision: proof.revision,
                    sensitivity: arsy_kernel::artifact::Sensitivity::Internal,
                    retain_until_ms: u64::MAX,
                },
            );
        }
        emitter.record(
            "task.proof",
            json!({
                "task": self.parent.to_string(),
                "state": proof.state,
                "outstanding": proof
                    .outstanding()
                    .iter()
                    .map(|criterion| json!({
                        "statement": criterion.statement,
                        "state": criterion.state,
                        "why": criterion.why,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
        if proof.state.is_verified() {
            let _ = graph.verify(
                self.parent,
                serde_json::to_value(&proof).unwrap_or(Value::Null),
            );
        }
    }

    /// Give every view back and delete the ones that hold nothing.
    ///
    /// A writer's branch survives `discard` when it has commits on it, so an
    /// integration the operator never ran is still reachable afterwards —
    /// the directory goes, the work does not.
    fn release_views(&mut self, emitter: &mut Emitter) {
        for child in std::mem::take(&mut self.children) {
            let Some(lease) = child.lease else {
                continue;
            };
            // Readers share one view per revision, so removing it would pull
            // the tree out from under a sibling still reading it.
            if lease.mutable {
                if let Err(error) = self.coordinator.discard(&lease) {
                    emitter.trace(
                        "subagent.view_retained",
                        json!({"view": lease.view.display().to_string(), "reason": error.to_string()}),
                    );
                }
            }
            if let Some(scheduler) = self.scheduler.as_mut() {
                let _ = scheduler
                    .graph_mut()
                    .release_workspace(child.assignment, "the turn that cut it ended");
            }
        }
    }

    /// Commit one acceptance criterion against this turn's task.
    ///
    /// Written to the graph, not held here: the point is that it is on record
    /// before the work, so it cannot be adjusted afterwards to fit whatever
    /// happened to pass.
    fn declare(&mut self, arguments: &Value, graph: &mut TaskGraph) -> Result<String, String> {
        let statement = arguments
            .get("statement")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if statement.is_empty() {
            return Err("a criterion needs a statement".into());
        }
        let check = arguments
            .get("check")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|check| !check.is_empty());
        let decided_by = arguments
            .get("decided_by")
            .and_then(Value::as_str)
            .unwrap_or(if check.is_some() { "check" } else { "review" });
        let verifier = match (decided_by, check) {
            ("check", Some(command)) => Verifier::Command {
                digest: arsy_kernel::validation::command_digest(command),
            },
            ("check", None) => {
                return Err("`decided_by: check` needs the command in `check`".into())
            }
            ("review", _) => Verifier::Review,
            ("human", _) => Verifier::Human,
            ("unverifiable", _) => Verifier::Unverifiable {
                why: arguments
                    .get("why_unverifiable")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            },
            (other, _) => return Err(format!("`decided_by` cannot be {other:?}")),
        };
        if matches!(&verifier, Verifier::Unverifiable { why } if why.is_empty()) {
            return Err("say why nothing can decide it".into());
        }
        let id = graph
            .declare_criterion(AcceptanceCriterion {
                id: CriterionId::new(),
                task: self.parent,
                statement: statement.clone(),
                verifier,
                required: arguments
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                freshness_ms: None,
                inapplicable: None,
            })
            .map_err(|error| format!("the criterion could not be recorded: {error}"))?;
        Ok(json!({
            "criterion": id.to_string(),
            "statement": statement,
            "note": "this is now on record; `arsy verify` will hold the work to it",
        })
        .to_string())
    }

    /// Start one child and return its ids without waiting for its answer.
    fn start(&mut self, arguments: &Value, graph: &mut TaskGraph) -> Result<String, String> {
        // Taken before anything can go wrong, so a spawn that fails still
        // spends its slot: a bound that only counted the children that worked
        // would let a model spawn failures without end, and each one costs the
        // rounds a child is allowed.
        self.allowance.take()?;
        let goal = arguments
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if goal.is_empty() {
            return Err("a subagent needs a goal to work towards".into());
        }
        let role = spawn_role(arguments)?;
        let contract = role.contract();
        let writer = match arguments.get("workspace").and_then(Value::as_str) {
            None => contract.workspace == WorkspaceRequirement::IsolatedWriter,
            Some("read_only_snapshot") => false,
            Some("isolated_writer") => true,
            Some(other) => {
                return Err(format!(
                    "`workspace` is \"read_only_snapshot\" or \"isolated_writer\", not {other:?}"
                ))
            }
        };
        let asked = if role == AgentRole::SafetyReviewer
            && arguments
                .get("capabilities")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        {
            Vec::new()
        } else {
            requested(&self.delegable, arguments)?
        };
        validate_role(role, writer, &asked)?;
        writing_needs_a_tree_of_its_own(writer, &asked)?;
        self.run_child(&goal, role, &asked, writer, graph)
    }

    fn status(&self) -> Result<String, String> {
        let children: Vec<Value> = self
            .children
            .iter()
            .map(|child| {
                json!({
                    "task": child.task.to_string(),
                    "goal": child.goal,
                    "role": role_name(child.role),
                    "state": self.state_of(child),
                    "answer_ready": child.worker.is_none(),
                })
            })
            .collect();
        Ok(json!({"subagents": children}).to_string())
    }

    /// Wait for children, up to a bound, and report what they said.
    fn wait(&mut self, arguments: &Value, emitter: &mut Emitter) -> Result<String, String> {
        let wanted = self.selection(arguments)?;
        let join = join_policy(arguments, wanted.len())?;
        let deadline = Instant::now()
            + Duration::from_millis(
                arguments
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_WAIT_MS)
                    .min(MAX_WAIT_MS),
            );

        // Polled rather than blocked on one handle at a time: `join` may be
        // satisfied by any of them, and waiting on the first would ignore the
        // second finishing first.
        loop {
            self.reap_finished(&wanted, emitter);
            let states: Vec<TaskState> = wanted
                .iter()
                .filter_map(|task| self.child(*task))
                .map(|child| self.task_state(child))
                .collect();
            if join.satisfied(states.iter().copied()) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Ok(json!({"subagents": self.answers(&wanted)}).to_string())
    }

    fn result(&mut self, arguments: &Value, emitter: &mut Emitter) -> Result<String, String> {
        let task = self.named(arguments)?;
        if let Some(index) = self.finished(&[task]).first() {
            self.reap(*index, emitter);
        }
        let child = self.child(task).ok_or_else(|| unknown(task))?;
        if child.worker.is_some() {
            return Err(format!(
                "subagent {task} has not finished; use `task.wait` or ask again later"
            ));
        }
        Ok(json!(self.answers(&[task]).first().cloned()).to_string())
    }

    fn stop(&mut self, arguments: &Value) -> Result<String, String> {
        let task = self.named(arguments)?;
        let reason = arguments
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("the parent no longer needs this answer")
            .to_owned();
        let (attempt, cancel) = {
            let child = self.child(task).ok_or_else(|| unknown(task))?;
            (child.attempt, child.cancel.clone())
        };
        // Recorded before the flag, so a process that dies between the two
        // comes back knowing the attempt was told to stop.
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler
                .cancel(attempt, reason.clone())
                .map_err(|error| format!("the cancellation could not be recorded: {error}"))?;
        }
        cancel.cancel();
        Ok(json!({"task": task.to_string(), "cancelling": reason}).to_string())
    }

    fn steer(&mut self, arguments: &Value) -> Result<String, String> {
        let task = self.named(arguments)?;
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if message.is_empty() {
            return Err("a message needs something in it".into());
        }
        let parent = self.parent;
        let scheduler = self
            .scheduler
            .as_mut()
            .ok_or_else(|| "this turn has no subagents".to_owned())?;
        let id = scheduler
            .graph_mut()
            .send(
                parent,
                task,
                MessageKind::Instruction,
                json!(message),
                None,
                arsy_kernel::artifact::unix_time_ms(),
            )
            .map_err(|error| format!("the message could not be recorded: {error}"))?;
        Ok(json!({"task": task.to_string(), "message": id.to_string()}).to_string())
    }

    /// Apply one writer's work to the workspace this turn owns.
    ///
    /// The integrator is the parent, and only the parent: a writer holds
    /// authority over its own view and nothing else, so the one agent that
    /// can change the canonical tree is the one that was never given an
    /// isolated one.
    fn integrate(&mut self, arguments: &Value) -> Result<String, String> {
        let task = self.named(arguments)?;
        let child = self.child(task).ok_or_else(|| unknown(task))?;
        if !child.writer {
            return Err(format!(
                "subagent {task} is a reader and produced no changes"
            ));
        }
        if child.worker.is_some() {
            return Err(format!("subagent {task} has not finished"));
        }
        let assignment = child.assignment;
        let authorized = self
            .delegable
            .iter()
            .any(|grant| grant.action == CapabilityAction::FsWrite)
            || self.mode == ExecutionMode::Normal;
        let scheduler = self
            .scheduler
            .as_ref()
            .ok_or_else(|| "this turn has no subagents".to_owned())?;
        let result = scheduler
            .graph()
            .writer_result(assignment)
            .cloned()
            .ok_or_else(|| format!("subagent {task} recorded no writer result"))?;
        let head = result
            .head_revision
            .clone()
            .ok_or_else(|| format!("subagent {task} committed nothing to integrate"))?;

        let outcome = self
            .coordinator
            .integrate(&IntegrationRequest {
                target: &self.root,
                writer_revision: &head,
                base_revision: &result.base_revision,
                changed_files: &result.changed_files,
                already_integrated: &self.integrated,
                validation: &result.validation,
                unresolved: &result.unresolved,
                expect_target_revision: arguments
                    .get("expect_target_revision")
                    .and_then(Value::as_str),
                allow_stale_base: false,
                policy_authorized: authorized,
            })
            .map_err(|error| format!("the change was not applied: {error}"))?;
        match outcome {
            MergeOutcome::Applied { commit } => {
                self.integrated.extend(result.changed_files.iter().cloned());
                Ok(json!({
                    "task": task.to_string(),
                    "applied": commit,
                    "changed_files": result.changed_files,
                })
                .to_string())
            }
            // The target is untouched: `integrate` aborts a merge it could
            // not finish, so a conflict is a report rather than a state.
            MergeOutcome::Conflict { paths, evidence } => Ok(json!({
                "task": task.to_string(),
                "conflict": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "evidence": evidence,
                "note": "the workspace is unchanged; resolve by respawning the writer from the current revision",
            })
            .to_string()),
        }
    }
}

/// The actions asked for, checked against what may be delegated at all.
fn requested(
    delegable: &[CapabilityGrant],
    arguments: &Value,
) -> Result<Vec<CapabilityAction>, String> {
    {
        let named: Vec<&str> = arguments
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|listed| listed.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| vec!["fs.read"]);
        if named.is_empty() {
            return Err("a subagent with no capability can do nothing; ask for `fs.read`".into());
        }
        let mut actions = BTreeSet::new();
        for name in named {
            let Some((_, action)) = DELEGABLE.iter().find(|(known, _)| *known == name) else {
                return Err(format!(
                    "`{name}` cannot be delegated; a subagent may ask for {}",
                    DELEGABLE
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            };
            if !delegable.iter().any(|grant| grant.action == *action) {
                return Err(format!(
                    "this workspace does not delegate `{name}`; a rule must grant it with a \
                     delegation depth above zero before a subagent can hold it"
                ));
            }
            actions.insert(*action);
        }
        Ok(actions.into_iter().collect())
    }
}

fn spawn_role(arguments: &Value) -> Result<AgentRole, String> {
    let inferred = || {
        let capabilities = arguments
            .get("capabilities")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if arguments.get("workspace").and_then(Value::as_str) == Some("isolated_writer")
            || capabilities.iter().any(|value| value == "fs.write")
        {
            "implementer"
        } else if capabilities.iter().any(|value| value == "process.exec") {
            "test_runner"
        } else {
            "explorer"
        }
    };
    match arguments
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_else(inferred)
    {
        "explorer" => Ok(AgentRole::Explorer),
        "planner" => Ok(AgentRole::Planner),
        "implementer" => Ok(AgentRole::Implementer),
        "debugger" => Ok(AgentRole::Debugger),
        "test_runner" => Ok(AgentRole::TestRunner),
        "reviewer" => Ok(AgentRole::Reviewer),
        "integrator" => Ok(AgentRole::Integrator),
        "safety_reviewer" => Ok(AgentRole::SafetyReviewer),
        other => Err(format!("unknown subagent role {other:?}")),
    }
}

const fn role_name(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Explorer => "explorer",
        AgentRole::Planner => "planner",
        AgentRole::Implementer => "implementer",
        AgentRole::Debugger => "debugger",
        AgentRole::TestRunner => "test_runner",
        AgentRole::Reviewer => "reviewer",
        AgentRole::Integrator => "integrator",
        AgentRole::SafetyReviewer => "safety_reviewer",
    }
}

fn validate_role(role: AgentRole, writer: bool, asked: &[CapabilityAction]) -> Result<(), String> {
    let contract = role.contract();
    if writer != (contract.workspace == WorkspaceRequirement::IsolatedWriter) {
        return Err(format!(
            "the {} role requires a {} workspace",
            role_name(role),
            if contract.workspace == WorkspaceRequirement::IsolatedWriter {
                "isolated_writer"
            } else {
                "read_only_snapshot"
            }
        ));
    }
    if let Some(action) = asked
        .iter()
        .find(|action| !contract.capabilities.contains(action))
    {
        return Err(format!(
            "the {} role contract does not permit `{}`",
            role_name(role),
            action.as_str()
        ));
    }
    Ok(())
}

impl Supervisor<'_> {
    /// Record the child, attenuate its authority, and hand it to a worker.
    fn run_child(
        &mut self,
        goal: &str,
        role: AgentRole,
        actions: &[CapabilityAction],
        writer: bool,
        graph: &mut TaskGraph,
    ) -> Result<String, String> {
        let agent = AgentId::new();
        let id = TaskId::new();
        let parent_budget = graph
            .node(self.parent)
            .map(|node| node.budget)
            .ok_or_else(|| "the parent task is not in its own graph".to_owned())?;
        // The parent's authority is written to the graph the first time it
        // delegates: a child's grants have to be explicable from the record,
        // and `add_child` attenuates from what the graph holds rather than
        // from whatever this process happens to be carrying.
        if graph
            .node(self.parent)
            .is_some_and(|node| node.authority.is_empty())
        {
            graph
                .authorize(self.parent, self.delegable.clone())
                .map_err(|error| {
                    format!("the parent's authority could not be recorded: {error}")
                })?;
        }
        let requests: Vec<ChildCapabilityRequest> = actions
            .iter()
            .filter_map(|action| {
                let index = self
                    .delegable
                    .iter()
                    .position(|grant| grant.action == *action)?;
                Some(ChildCapabilityRequest {
                    parent_grant: index,
                    action: *action,
                    // The workspace, and no wider: a child inherits the
                    // parent's scope narrowed by this, never widened.
                    scope: ResourceScope::single(
                        ResourcePattern::new(action.default_scheme(), "**").ok()?,
                    ),
                    expires_at_ms: self.delegable[index].expires_at_ms,
                })
            })
            .collect();
        if requests.len() != actions.len() {
            return Err("a requested capability could not be attenuated from the parent".into());
        }

        graph
            .add_child(
                self.parent,
                TaskNode {
                    id,
                    goal: goal.to_owned(),
                    dependencies: Vec::new(),
                    assignee: Some(agent),
                    required_output: required_output(writer),
                    workspace: if writer {
                        WorkspaceRequirement::IsolatedWriter
                    } else {
                        WorkspaceRequirement::ReadOnlySnapshot
                    },
                    budget: share(parent_budget),
                    authority: Vec::new(),
                    state: arsy_kernel::orchestration::TaskState::Pending,
                    lease_expires_at_ms: None,
                    runtime: Default::default(),
                },
                requests,
            )
            .map_err(|error| format!("the subagent could not be recorded: {error}"))?;
        graph
            .ready()
            .map_err(|error| format!("the subagent could not be queued: {error}"))?;

        // One scheduler per turn, over its own view of the same stream. Built
        // here rather than in `new` so a turn that never delegates never
        // replays the session a second time.
        if self.scheduler.is_none() {
            let forked = graph
                .fork(Principal::Agent(agent))
                .map_err(|error| format!("the subagent runtime could not start: {error}"))?;
            self.scheduler = Some(Scheduler::new(
                forked,
                // Concurrency, not spend: the budget was already reserved
                // against the parent when the child node was created.
                Admission::new()
                    .limit(SUBAGENT_SLOT, MAX_CONCURRENT_CHILDREN)
                    .limit(
                        provider_slot(&self.resolved.endpoint.id),
                        MAX_CONCURRENT_CHILDREN,
                    ),
                arsy_kernel::scheduler::RetryPolicy::default(),
            ));
        }
        let budget = share(parent_budget);
        let scheduler = self.scheduler.as_mut().expect("just built");
        let admitted = scheduler
            .start(
                id,
                &AttemptRequest {
                    role: role_name(role).to_owned(),
                    assignee: agent,
                    model: Some(arsy_kernel::orchestration::ModelDecision {
                        profile: self.resolved.endpoint.id.clone(),
                        model: self.model.clone(),
                    }),
                    base_revision: None,
                    started_at_ms: arsy_kernel::artifact::unix_time_ms(),
                    lease_expires_at_ms: arsy_kernel::artifact::unix_time_ms() + budget.wall_ms,
                },
                vec![
                    SUBAGENT_SLOT.to_owned(),
                    provider_slot(&self.resolved.endpoint.id),
                ],
            )
            .map_err(|error| match error {
                SchedulerError::Deferred(key) => format!(
                    "no free slot for {key}; wait for a running subagent before starting another"
                ),
                SchedulerError::Graph(error) => {
                    format!("the subagent could not be started: {error}")
                }
            })?;

        let granted = scheduler
            .graph()
            .node(id)
            .map(|node| node.authority.clone())
            .unwrap_or_default();
        let lease_epoch = scheduler
            .graph()
            .attempt(admitted.attempt)
            .map_or(0, |attempt| attempt.lease_epoch);

        // The view the child actually runs in. A writer gets a tree of its
        // own; a reader gets an immutable one pinned to a revision, so it is
        // not reading a workspace someone else is changing underneath it.
        let owner = ViewOwner {
            session: scheduler.graph().session(),
            task: id,
            attempt: admitted.attempt,
            agent,
        };
        let lease = match self.lease_view(&owner, writer) {
            Ok(lease) => lease,
            Err(error) => {
                let scheduler = self.scheduler.as_mut().expect("just built");
                let _ = scheduler.cancel(admitted.attempt, "no isolated view could be cut");
                return Err(format!("the subagent has no workspace to work in: {error}"));
            }
        };
        let scheduler = self.scheduler.as_mut().expect("just built");
        let assignment = scheduler
            .graph_mut()
            .assign_workspace(WorkspaceAssignment {
                id: AssignmentId::new(),
                task: id,
                attempt: admitted.attempt,
                owner: agent,
                repository: WorkspaceCoordinator::repository_identity(&self.root)
                    .unwrap_or_else(|_| self.root.display().to_string()),
                source: self.root.display().to_string(),
                base_revision: lease.base_revision.clone(),
                view: lease.view.display().to_string(),
                backend: match lease.backend {
                    arsy_code::workspace::IsolationBackend::GitWorktree => {
                        IsolationBackend::GitWorktree
                    }
                    arsy_code::workspace::IsolationBackend::CopiedSnapshot => {
                        IsolationBackend::CopiedSnapshot
                    }
                },
                mutable: writer,
                lease_epoch,
                expires_at_ms: arsy_kernel::artifact::unix_time_ms() + budget.wall_ms,
                dirty: match &lease.carried_patch {
                    None => DirtyDisposition::Clean,
                    Some(patch) => DirtyDisposition::CapturedPatch {
                        artifact: format!("inline:{} bytes", patch.len()),
                    },
                },
                released: false,
            })
            .map_err(|error| format!("the subagent's workspace could not be recorded: {error}"))?;

        let worker = Worker {
            lease: lease.clone(),
            writer,
            assignment,
            artifacts_root: self.root.join(arsy_code::workspace::ARTIFACTS),
            config: self.config.clone(),
            provider: Arc::clone(&self.resolved.provider),
            endpoint: self.resolved.endpoint.id.clone(),
            endpoint_config: self.resolved.endpoint.clone(),
            model: self.model.clone(),
            mode: self.mode,
            goal: goal.to_owned(),
            role,
            agent,
            task: id,
            attempt: admitted.attempt,
            granted,
            cancel: admitted.cancel.clone(),
            graph: scheduler
                .graph()
                .fork(Principal::Agent(agent))
                .map_err(|error| format!("the subagent runtime could not start: {error}"))?,
        };
        let handle = std::thread::Builder::new()
            .name(format!("arsy-subagent-{agent}"))
            .spawn(move || worker.run())
            .map_err(|error| format!("the subagent thread could not start: {error}"))?;

        self.children.push(Child {
            task: id,
            attempt: admitted.attempt,
            goal: goal.to_owned(),
            role,
            cancel: admitted.cancel,
            worker: Some(handle),
            report: None,
            lease: Some(lease),
            assignment,
            writer,
        });
        Ok(json!({
            "task": id.to_string(),
            "attempt": admitted.attempt.to_string(),
            "started": goal,
            "role": role_name(role),
            "workspace": if writer { "isolated_writer" } else { "read_only_snapshot" },
            "note": "running; read it back with `task.wait` or `task.result`",
        })
        .to_string())
    }

    /// Cut the view this child runs in.
    fn lease_view(
        &mut self,
        owner: &ViewOwner,
        writer: bool,
    ) -> Result<WorkspaceLease, arsy_code::workspace::WorkspaceError> {
        let views = self.root.join(VIEW_DIRECTORY);
        if writer {
            self.coordinator.writer_for(
                &self.root,
                &views,
                owner,
                arsy_kernel::artifact::unix_time_ms() + WRITER_LEASE_MS,
                // The operator decides what happens to their own uncommitted
                // work. Carrying it silently into a writer's tree would put
                // changes nobody delegated into a diff nobody reviewed.
                DirtyPolicy::Refuse,
            )
        } else {
            self.coordinator.reader(&self.root, &views, owner.agent)
        }
    }

    fn child(&self, task: TaskId) -> Option<&Child> {
        self.children.iter().find(|child| child.task == task)
    }

    /// The task ids this call is about: those named, or all of them.
    fn selection(&self, arguments: &Value) -> Result<Vec<TaskId>, String> {
        let Some(named) = arguments.get("tasks").and_then(Value::as_array) else {
            return Ok(self.children.iter().map(|child| child.task).collect());
        };
        named
            .iter()
            .filter_map(Value::as_str)
            .map(|id| {
                id.parse::<TaskId>()
                    .map_err(|_| format!("{id} is not a task id"))
                    .and_then(|task| self.child(task).map(|_| task).ok_or_else(|| unknown(task)))
            })
            .collect()
    }

    fn named(&self, arguments: &Value) -> Result<TaskId, String> {
        let id = arguments
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| "name the subagent's task id".to_owned())?;
        let task = id
            .parse::<TaskId>()
            .map_err(|_| format!("{id} is not a task id"))?;
        self.child(task).map(|_| task).ok_or_else(|| unknown(task))
    }

    /// Join every named child whose worker has stopped.
    fn reap_finished(&mut self, wanted: &[TaskId], emitter: &mut Emitter) {
        for index in self.finished(wanted) {
            self.reap(index, emitter);
        }
    }

    /// Indices of children whose worker has finished but not been reaped.
    fn finished(&self, wanted: &[TaskId]) -> Vec<usize> {
        self.children
            .iter()
            .enumerate()
            .filter(|(_, child)| wanted.contains(&child.task))
            .filter(|(_, child)| child.worker.as_ref().is_some_and(JoinHandle::is_finished))
            .map(|(index, _)| index)
            .collect()
    }

    /// Join one finished worker and settle its attempt.
    ///
    /// Settling here rather than on the worker keeps the terminal record in
    /// one place: whatever the thread did — answered, failed, panicked — the
    /// attempt ends with a reason the parent can explain.
    fn reap(&mut self, index: usize, emitter: &mut Emitter) {
        let Some(handle) = self.children[index].worker.take() else {
            return;
        };
        let attempt = self.children[index].attempt;
        let report = handle.join().unwrap_or_else(|_| Report {
            answer: Err("the subagent thread stopped unexpectedly".to_owned()),
            interventions: Vec::new(),
        });
        self.interventions.extend(report.interventions.clone());
        for intervention in &report.interventions {
            emitter.trace("subagent.intervention", intervention.clone());
        }
        if let Some(scheduler) = self.scheduler.as_mut() {
            let now = arsy_kernel::artifact::unix_time_ms();
            let outcome = match &report.answer {
                Ok(answer) => AttemptOutcome::completed(Budget::default(), json!(answer), now),
                Err(reason) if reason == crate::CHILD_CANCELLED => AttemptOutcome {
                    state: AttemptState::Cancelled,
                    used: Budget::default(),
                    reason: Some(reason.clone()),
                    retryable: Retryability::NotRetryable,
                    result: None,
                    evidence: Vec::new(),
                    ended_at_ms: now,
                },
                Err(reason) => AttemptOutcome::failed(Budget::default(), reason.clone(), now),
            };
            // Already ended by the worker in the ordinary case; this closes
            // the ones where the worker could not, such as a panic.
            let _ = scheduler.finish(attempt, &outcome);
        }
        self.children[index].report = Some(report);
    }

    /// What one child is, as a word the model can act on.
    fn state_of(&self, child: &Child) -> &'static str {
        if child.worker.is_some() {
            return if child.cancel.is_cancelled() {
                "cancelling"
            } else {
                "running"
            };
        }
        match child.report.as_ref().map(|report| &report.answer) {
            Some(Ok(_)) => "answered",
            Some(Err(reason)) if reason == crate::CHILD_CANCELLED => "cancelled",
            Some(Err(_)) => "failed",
            None => "starting",
        }
    }

    /// The same, as the task state a join policy reads.
    fn task_state(&self, child: &Child) -> TaskState {
        match self.state_of(child) {
            "answered" => TaskState::Completed,
            "failed" => TaskState::Failed,
            "cancelled" => TaskState::Cancelled,
            _ => TaskState::Running,
        }
    }

    fn answers(&self, wanted: &[TaskId]) -> Vec<Value> {
        self.children
            .iter()
            .filter(|child| wanted.contains(&child.task))
            .map(|child| {
                let mut entry = json!({
                    "task": child.task.to_string(),
                    "goal": child.goal,
                    "state": self.state_of(child),
                });
                match child.report.as_ref().map(|report| &report.answer) {
                    Some(Ok(answer)) => entry["answer"] = json!(answer),
                    Some(Err(reason)) => entry["reason"] = json!(reason),
                    None => {}
                }
                entry
            })
            .collect()
    }
}

/// Everything one child needs, owned, so it can run on its own thread.
struct Worker {
    /// The view this child runs in. Its path is the runtime root, which is
    /// what confines every path the child can name.
    lease: WorkspaceLease,
    writer: bool,
    assignment: AssignmentId,
    /// The canonical workspace's artifact store, not the view's. Evidence has
    /// to outlive the tree it was produced in, and a read-only reader view
    /// has nowhere to put it anyway.
    artifacts_root: PathBuf,
    config: Config,
    provider: Arc<dyn arsy_kernel::provider::ModelProvider>,
    endpoint: String,
    endpoint_config: arsy_kernel::config::Endpoint,
    model: String,
    mode: ExecutionMode,
    goal: String,
    role: AgentRole,
    agent: AgentId,
    task: TaskId,
    attempt: AttemptId,
    granted: Vec<CapabilityGrant>,
    cancel: CancelToken,
    /// The worker's own view of the session's stream, so what the child did
    /// is durable as it happens rather than only when the parent reaps it.
    graph: TaskGraph,
}

impl Worker {
    fn run(mut self) -> Report {
        let mut observer = ObserverSubscription {
            id: SubscriptionId::new(),
            observer: AgentId::new(),
            authority: ObserverAuthority {
                // The supervisor may stop a child it is paying for. It may not
                // do anything else: an observer that could act would be an
                // agent, and this one has no tools.
                may_suggest: true,
                may_deny: true,
            },
            cost_budget_micros: OBSERVER_BUDGET,
            cost_used_micros: 0,
        };
        let mut interventions = Vec::new();
        let _ = self
            .graph
            .advance_attempt(self.attempt, AttemptState::Running);
        let mut tokens = (0u64, 0u64);
        let answer = self.execute(&mut observer, &mut interventions, &mut tokens);

        // Settled by the worker, so a parent that never reaps still leaves a
        // terminal record rather than a lease waiting to expire.
        let now = arsy_kernel::artifact::unix_time_ms();
        let used = Budget {
            tokens: tokens.0.saturating_add(tokens.1),
            cost_micros: 0,
            wall_ms: 0,
        };
        // A writer answers with the manifest its task asked for, so its
        // result is read as structure rather than stored as a sentence that
        // happens to look like JSON.
        let structured = answer
            .as_ref()
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .filter(Value::is_object);
        if self.writer {
            // Committed before the result is built, so the revision the
            // manifest names is one Git can hand to an integrator rather
            // than a working tree that vanishes with the view.
            match WorkspaceCoordinator::commit_view(&self.lease, &format!("arsy: {}", self.goal)) {
                Ok(_) => {}
                Err(error) => {
                    let _ = self.graph.record(
                        self.attempt,
                        "writer.commit_failed",
                        json!(error.to_string()),
                    );
                }
            }
            if let Some(result) = structured.as_ref() {
                let _ = self.graph.record_writer_result(&self.writer_result(result));
            }
        }
        let outcome = match &answer {
            Ok(text) => {
                AttemptOutcome::completed(used, structured.unwrap_or_else(|| json!(text)), now)
            }
            Err(reason) if reason == crate::CHILD_CANCELLED => AttemptOutcome {
                state: AttemptState::Cancelled,
                used,
                reason: Some(reason.clone()),
                retryable: Retryability::NotRetryable,
                result: None,
                evidence: Vec::new(),
                ended_at_ms: now,
            },
            // A provider fault is the one thing worth trying again; anything
            // else a child reports is its own answer about the workspace.
            Err(reason) => {
                AttemptOutcome::failed(used, reason.clone(), now).retryable(Retryability::Retryable)
            }
        };
        let _ = self.graph.finish_attempt(self.attempt, &outcome);
        Report {
            answer,
            interventions,
        }
    }

    /// What the writer produced, as the thing an integrator reads.
    ///
    /// The revision and the changed files come from Git rather than from the
    /// answer: a writer naming files it did not touch, or a head it did not
    /// commit, would be describing work the integrator then applies blind.
    /// Only the parts Git cannot know — which checks were run, what was left
    /// unsettled — are taken from what the child said.
    fn writer_result(&self, answer: &Value) -> WriterResult {
        let strings = |key: &str| {
            answer
                .get(key)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let head = arsy_code::git::head_revision(&self.lease.view).ok();
        let changed = head
            .as_deref()
            .and_then(|head| {
                arsy_code::git::changed_between(&self.lease.view, &self.lease.base_revision, head)
                    .ok()
            })
            .unwrap_or_default();
        WriterResult {
            assignment: self.assignment,
            base_revision: self.lease.base_revision.clone(),
            head_revision: head,
            patch_artifact: None,
            changed_files: changed,
            validation: strings("validation"),
            unresolved: strings("unresolved"),
        }
    }

    /// One child turn, under a runtime that can do only what the child holds.
    fn execute(
        &mut self,
        observer: &mut ObserverSubscription,
        interventions: &mut Vec<Value>,
        tokens: &mut (u64, u64),
    ) -> Result<String, String> {
        let workspace = arsy_code::resource::Workspace::open(&self.lease.view)
            .map_err(|error| error.to_string())?;
        let artifacts = Arc::new(
            arsy_kernel::artifact::FileArtifactStore::open(self.artifacts_root.clone(), 0)
                .map_err(|error| error.to_string())?,
        );
        let runtime = arsy_code::agent::runtime(
            &workspace,
            // The child's grants, as rules. Expressing them in the engine's own
            // vocabulary means the child is authorized by the same code path
            // the parent is, rather than by a second implementation that could
            // disagree with it.
            rules_from(&self.granted),
            artifacts,
            arsy_kernel::artifact::unix_time_ms(),
            Principal::Agent(self.agent),
            RiskContext {
                reversible: false,
                workspace: arsy_code::git::cleanliness(&self.lease.view)
                    .unwrap_or(arsy_kernel::policy::WorkspaceCleanliness::Unknown),
                sandbox: crate::installed_sandbox_assurance(),
            },
            arsy_code::operations::Reachable::from_config(&self.config),
            // The child's own plan and validation history, not the parent's:
            // a delegated subtask should not inherit or pollute the plan the
            // supervisor is tracking.
            &self.agent.to_string(),
            // A delegate reports back to its parent; the parent owns the
            // session's checklist, so a child does not get one of its own.
            arsy_code::operations::TurnState::default(),
            // The child follows the same prompt its parent was given, so it
            // reads the same skills — none here, because this registry never
            // reaches a prompt.
            &[],
        )
        .map_err(|error| error.to_string())?
        .with_execution_mode(self.mode);

        let request = CanonicalModelRequest {
            model: ModelKey {
                provider: self.endpoint.clone(),
                model: self.model.clone(),
            },
            system: Some(child_instructions(self.role, &self.goal, self.writer)),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: self.goal.clone(),
                }],
            }],
            tools: if self.role == AgentRole::SafetyReviewer {
                Vec::new()
            } else {
                runtime.schemas()
            },
            max_output_tokens: self.endpoint_config.output_tokens_for(&self.model),
            effort: None,
            idempotency_key: IdempotencyKey::new(self.agent.to_string())
                .map_err(|error| error.to_string())?,
        };

        let (attempt, task) = (self.attempt, self.task);
        // Two callbacks write to one graph, and `child_turn` holds both at
        // once. They never run nested — one is called between rounds and the
        // other inside a round — so the check is a formality the borrow
        // checker cannot see for itself.
        let graph = std::cell::RefCell::new(&mut self.graph);
        crate::child_turn(
            self.provider.as_ref(),
            &self.endpoint_config,
            &runtime,
            &request,
            &mut |projection| watch(observer, interventions, projection),
            &mut |kind, data| {
                let _ = graph.borrow_mut().record(attempt, kind, data);
            },
            &mut || {
                graph
                    .borrow_mut()
                    .deliver(task, arsy_kernel::artifact::unix_time_ms())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|message| {
                        message
                            .body
                            .as_str()
                            .map(|text| format!("Your parent sent you this: {text}"))
                    })
                    .collect()
            },
            &mut || loop {
                let mut graph = graph.borrow_mut();
                if graph.refresh().is_err()
                    || graph
                        .attempt(attempt)
                        .is_some_and(|attempt| attempt.state == AttemptState::Cancelling)
                {
                    return false;
                }
                if !graph.is_paused(attempt) {
                    return true;
                }
                drop(graph);
                std::thread::sleep(POLL_INTERVAL);
            },
            &self.cancel,
            tokens,
        )
    }
}

/// Offer one redacted projection of the child's activity to its observer.
///
/// Returns the intervention the observer made, if any. A `Deny` stops the
/// child; a `Suggest` is recorded and reaches the parent with the answer.
fn watch(
    observer: &mut ObserverSubscription,
    interventions: &mut Vec<Value>,
    projection: &RedactedProjection,
) -> Option<Intervention> {
    let intervention = warranted(projection)?;
    match observer.intervene(projection, intervention.clone(), 1) {
        Ok(event) => {
            interventions.push(json!({
                "subscription": event.subscription.to_string(),
                "at_sequence": event.projection_sequence,
                "intervention": event.intervention,
                "cost_micros": event.cost_micros,
            }));
            Some(intervention)
        }
        // Out of budget or out of authority: the observer stops observing
        // rather than acting beyond what it was given.
        Err(_) => None,
    }
}

/// Writing is only ever delegated into a tree of the child's own.
///
/// A reader shares the parent's revision — the point of a read-only snapshot
/// is that it is the same tree the parent is reasoning about — so a reader
/// that could write would be writing the workspace its parent is working in,
/// which is the thing isolation exists to prevent.
fn writing_needs_a_tree_of_its_own(writer: bool, asked: &[CapabilityAction]) -> Result<(), String> {
    (writer || !asked.contains(&CapabilityAction::FsWrite))
        .then_some(())
        .ok_or_else(|| {
            "`fs.write` needs `\"workspace\": \"isolated_writer\"`; a reader shares the parent's \
             revision and must not change it"
                .to_owned()
        })
}

fn unknown(task: TaskId) -> String {
    format!("this turn started no subagent {task}")
}

fn provider_slot(endpoint: &str) -> String {
    format!("provider:{endpoint}")
}

/// What `join` asks for, defaulting to every task named.
fn join_policy(arguments: &Value, tasks: usize) -> Result<JoinPolicy, String> {
    match arguments.get("join").and_then(Value::as_str) {
        None | Some("all") => Ok(JoinPolicy::Quorum(tasks.max(1))),
        Some("any") => Ok(JoinPolicy::Any),
        Some(count) => count
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .map(JoinPolicy::Quorum)
            .ok_or_else(|| format!("`join` is \"all\", \"any\", or a count, not {count:?}")),
    }
}

/// What a child has to produce for its attempt to count as completed.
///
/// A writer's is a schema rather than a sentence, so the graph can refuse an
/// implementer that answers prose where an integrator needs a manifest.
fn required_output(writer: bool) -> String {
    if !writer {
        return "an answer the parent can act on".to_owned();
    }
    json!({
        "type": "object",
        "required": ["summary", "changed_files", "validation", "unresolved"],
    })
    .to_string()
}

/// Where isolated views live, inside the workspace so they share its disk and
/// its cleanup.
const VIEW_DIRECTORY: &str = arsy_code::workspace::VIEWS;

/// How long a writer holds its tree before recovery may reclaim it.
const WRITER_LEASE_MS: u64 = 30 * 60 * 1_000;

/// The slot every child takes, whichever provider it uses.
const SUBAGENT_SLOT: &str = "subagent";

/// Children that may run at once. The same bound as the per-turn allowance,
/// so the limit an operator reasons about is one number rather than two.
const MAX_CONCURRENT_CHILDREN: u64 = MAX_CHILDREN as u64;

/// How long `task.wait` waits when the model does not say.
const DEFAULT_WAIT_MS: u64 = 60_000;

/// And the longest it will, whatever the model asks for: a turn that blocks
/// past this is one an operator cannot interrupt.
const MAX_WAIT_MS: u64 = 300_000;

/// How often a wait looks at its children. Short enough that an answer is not
/// left sitting, long enough that waiting is not a spin.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// What this projection warrants, if anything.
///
/// The rule is small and explainable on purpose: a child whose calls keep
/// failing is not working, and stopping it is worth more than the rounds it
/// would spend proving that.
///
/// The count comes from the payload, not from the sequence: the sequence is
/// which call this was, and an intervention that recorded a failure count in
/// its place would be a false entry in the audit.
fn warranted(projection: &RedactedProjection) -> Option<Intervention> {
    let failures = projection
        .public_payload
        .get("consecutive_failures")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (projection.kind == "tool.failed" && failures >= CONSECUTIVE_FAILURES)
        .then(|| Intervention::Deny(format!("{failures} tool calls in a row failed")))
}

/// A quarter of what is left, and at least enough to be worth starting.
fn share(parent: Budget) -> Budget {
    Budget {
        tokens: parent.tokens / CHILD_BUDGET_SHARE,
        cost_micros: parent.cost_micros / CHILD_BUDGET_SHARE,
        wall_ms: parent.wall_ms / CHILD_BUDGET_SHARE,
    }
}

/// Grants as the rules that admit exactly them.
fn rules_from(granted: &[CapabilityGrant]) -> RuleSet {
    RuleSet::compile(granted.iter().flat_map(|grant| {
        grant
            .scope
            .patterns()
            .iter()
            .map(|pattern| PolicyRule {
                // The parent's own authority is what this came from, so it
                // carries the parent's source rather than claiming more.
                source: grant.source,
                effect: RuleEffect::Allow,
                actor: ActorMatch::Exactly(grant.actor.clone()),
                action: grant.action,
                pattern: pattern.clone(),
                expires_at_ms: grant.expires_at_ms,
                delegation_depth: grant.delegation_depth,
                minimum_assurance: SandboxAssurance::None,
            })
            .collect::<Vec<_>>()
    }))
}

/// What a child is told about its own position.
fn child_instructions(role: AgentRole, goal: &str, writer: bool) -> String {
    format!(
        "You are a subagent in the {role} role, responsible for this bounded task: {goal}\n\n\
         Your tools are limited by the role contract and delegated policy. {workspace} \
         Do not claim capabilities you were not given. If the answer is not available, say so \
         rather than guessing.\n",
        role = role_name(role),
        workspace = if writer {
            "Write only in your isolated workspace and return the required JSON manifest."
        } else {
            "This workspace is read-only."
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_may_ask_only_for_what_can_be_delegated() {
        use arsy_kernel::capability::PolicySource;
        let grant = |action: CapabilityAction| CapabilityGrant {
            id: arsy_kernel::domain::GrantId::new(),
            actor: Principal::User("dev".into()),
            action,
            scope: ResourceScope::single(
                ResourcePattern::new(action.default_scheme(), "**").unwrap(),
            ),
            expires_at_ms: None,
            delegation_depth: 1,
            source: PolicySource::User,
        };
        let supervisor = |delegable: Vec<CapabilityGrant>| Requested { delegable };

        // Nothing outside the list, whatever the parent holds.
        let all = supervisor(vec![
            grant(CapabilityAction::FsRead),
            grant(CapabilityAction::FsWrite),
        ]);
        assert!(all
            .requested(&json!({"capabilities": ["net.fetch"]}))
            .unwrap_err()
            .contains("cannot be delegated"));

        // Writing is delegable, but only into a tree of the child's own: a
        // reader shares the parent's revision, and must not change it.
        let write = all
            .requested(&json!({"capabilities": ["fs.write"]}))
            .unwrap();
        assert!(writing_needs_a_tree_of_its_own(false, &write)
            .unwrap_err()
            .contains("isolated_writer"));
        assert!(writing_needs_a_tree_of_its_own(true, &write).is_ok());

        // On the list, but this workspace delegates nothing.
        let none = supervisor(Vec::new());
        assert!(none
            .requested(&json!({"capabilities": ["fs.read"]}))
            .unwrap_err()
            .contains("does not delegate"));

        // The default is the narrowest thing that is useful.
        let reader = supervisor(vec![grant(CapabilityAction::FsRead)]);
        assert_eq!(
            reader.requested(&json!({})).unwrap(),
            vec![CapabilityAction::FsRead]
        );
        assert!(reader
            .requested(&json!({"capabilities": []}))
            .unwrap_err()
            .contains("no capability"));
    }

    #[test]
    fn role_contracts_bound_workspace_and_capabilities() {
        assert!(validate_role(AgentRole::Explorer, false, &[CapabilityAction::FsRead]).is_ok());
        assert!(
            validate_role(AgentRole::Explorer, false, &[CapabilityAction::ProcessExec])
                .unwrap_err()
                .contains("does not permit")
        );
        assert!(
            validate_role(AgentRole::Implementer, false, &[CapabilityAction::FsWrite])
                .unwrap_err()
                .contains("isolated_writer")
        );
        assert!(validate_role(AgentRole::SafetyReviewer, false, &[]).is_ok());
    }

    #[test]
    fn a_childs_rules_admit_its_grants_and_nothing_else() {
        let agent = AgentId::new();
        let read = CapabilityGrant {
            id: arsy_kernel::domain::GrantId::new(),
            actor: Principal::Agent(agent),
            action: CapabilityAction::FsRead,
            scope: ResourceScope::single(ResourcePattern::new("file", "**").unwrap()),
            expires_at_ms: None,
            delegation_depth: 0,
            source: arsy_kernel::capability::PolicySource::User,
        };
        let rules = rules_from(&[read]);

        let query = |action: CapabilityAction, actor: Principal| arsy_kernel::policy::PolicyQuery {
            actor,
            operation: arsy_kernel::operation::OperationKind::new("fs.read").unwrap(),
            requirement: arsy_kernel::capability::CapabilityRequirement {
                action,
                resource: arsy_kernel::domain::ResourceRef::new(
                    action.default_scheme(),
                    "src/lib.rs",
                )
                .unwrap(),
            },
            operation_digest: arsy_kernel::domain::StateVersion::from_digest([0; 32]),
            resource_version: None,
            context: RiskContext {
                reversible: true,
                workspace: arsy_kernel::policy::WorkspaceCleanliness::Clean,
                sandbox: SandboxAssurance::None,
            },
        };

        assert!(rules
            .evaluate(&query(CapabilityAction::FsRead, Principal::Agent(agent)))
            .decision
            .is_allow());
        // Writing was never granted, so the child's own runtime refuses it.
        assert!(!rules
            .evaluate(&query(CapabilityAction::FsWrite, Principal::Agent(agent)))
            .decision
            .is_allow());
        // And the grant is the child's, not anyone else's.
        assert!(!rules
            .evaluate(&query(
                CapabilityAction::FsRead,
                Principal::User("dev".into())
            ))
            .decision
            .is_allow());
    }

    #[test]
    fn a_child_budget_is_a_share_of_what_the_parent_has_left() {
        let parent = Budget {
            tokens: 400,
            cost_micros: 40,
            wall_ms: 4_000,
        };
        let child = share(parent);
        assert_eq!(child.tokens, 100);
        assert!(
            child.fits_within(parent),
            "a child never exceeds its parent"
        );
    }

    /// A projection as `child_turn` builds one.
    fn projection(call: u64, failures: u64, kind: &str) -> RedactedProjection {
        RedactedProjection {
            sequence: call,
            kind: kind.to_owned(),
            public_payload: json!({"tool": "fs.read", "consecutive_failures": failures}),
            redacted_fields: 2,
        }
    }

    #[test]
    fn the_sequence_is_which_call_it_was_and_the_count_lives_in_the_payload() {
        // `warranted` is the rule the supervisor runs, called here rather than
        // copied: a test that reimplements the rule passes against a
        // supervisor that reads the wrong field.
        assert!(
            warranted(&projection(10, 2, "tool.failed")).is_none(),
            "two in a row is not yet a pattern"
        );
        assert!(
            warranted(&projection(11, 0, "tool.completed")).is_none(),
            "a success is never an intervention"
        );

        match warranted(&projection(12, 3, "tool.failed")) {
            // The reason names the count, not the call number: the two were
            // once the same field, and that made the audit trail wrong.
            Some(Intervention::Deny(reason)) => assert!(reason.starts_with("3 "), "{reason}"),
            other => panic!("three in a row is denied, not {other:?}"),
        }
    }

    #[test]
    fn an_observer_stops_a_child_that_keeps_failing_and_stops_when_out_of_budget() {
        let mut observer = ObserverSubscription {
            id: SubscriptionId::new(),
            observer: AgentId::new(),
            authority: ObserverAuthority {
                may_suggest: true,
                may_deny: true,
            },
            cost_budget_micros: 1,
            cost_used_micros: 0,
        };
        let projection = RedactedProjection {
            sequence: 3,
            kind: "tool.failed".to_owned(),
            public_payload: json!({"tool": "fs.read"}),
            redacted_fields: 1,
        };

        assert!(observer
            .intervene(&projection, Intervention::Deny("failing".into()), 1)
            .is_ok());
        // The budget is spent, so it observes without acting rather than
        // acting without a budget.
        assert!(observer
            .intervene(&projection, Intervention::Deny("failing".into()), 1)
            .is_err());
    }

    /// The half of [`Supervisor`] these tests exercise, without a provider or a
    /// workspace behind it.
    struct Requested {
        delegable: Vec<CapabilityGrant>,
    }

    impl Requested {
        fn requested(&self, arguments: &Value) -> Result<Vec<CapabilityAction>, String> {
            super::requested(&self.delegable, arguments)
        }
    }

    #[test]
    fn every_spawn_counts_against_the_bound_including_the_ones_that_fail() {
        // Driving the real type, not a copy of its logic: the bug this covers
        // was a bound that was checked and never incremented, and a test that
        // counted for itself would have passed against it.
        let mut allowance = Allowance::default();
        for _ in 0..MAX_CHILDREN {
            allowance.take().expect("the allowance is not spent yet");
        }
        let refused = allowance
            .take()
            .expect_err("the allowance is spent, whatever the children did with it");
        assert!(refused.contains("already spawned"), "{refused}");
    }
}
