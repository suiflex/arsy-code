//! The task graph: what was delegated, to whom, under what authority, and
//! what each try of it actually spent.
//!
//! Everything here is a projection of one session's event stream. A restarted
//! process rebuilds task lineage, attempt state, authority derivation, budget
//! reserved and used, and terminal reasons by replaying that stream — there is
//! no second scheduler database to disagree with it.

use crate::{
    capability::{AttenuationError, CapabilityAction, CapabilityGrant, ResourceScope},
    domain::{
        AgentId, AssignmentId, AttemptId, CorrelationId, CriterionId, MessageId, Principal,
        SessionId, StateVersion, TaskId, WorkspaceVersion,
    },
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
    todo::TodoItem,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

/// Wire version of [`TaskAttempt`]. A reader that meets a higher version knows
/// it is reading a record it does not fully understand, rather than silently
/// dropping the fields it has no name for.
pub const ATTEMPT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Budget {
    pub tokens: u64,
    pub cost_micros: u64,
    pub wall_ms: u64,
}

impl Budget {
    pub const fn fits_within(self, parent: Self) -> bool {
        self.tokens <= parent.tokens
            && self.cost_micros <= parent.cost_micros
            && self.wall_ms <= parent.wall_ms
    }

    /// Saturating because a budget is a bound, not an arithmetic result: an
    /// accounting overflow must not wrap into a larger allowance.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            tokens: self.tokens.saturating_add(other.tokens),
            cost_micros: self.cost_micros.saturating_add(other.cost_micros),
            wall_ms: self.wall_ms.saturating_add(other.wall_ms),
        }
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            tokens: self.tokens.saturating_sub(other.tokens),
            cost_micros: self.cost_micros.saturating_sub(other.cost_micros),
            wall_ms: self.wall_ms.saturating_sub(other.wall_ms),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PartialEvidence {
    pub reason: String,
    pub remaining: Budget,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRequirement {
    ReadOnlySnapshot,
    IsolatedWriter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    /// A dependency ended without completing, so this task can never become
    /// ready. Distinct from `Failed`: nothing of this task ran.
    Blocked,
    Running,
    /// Execution finished. That is not the same claim as `Verified`: a model
    /// turn that returned, or a process that exited zero, says the work ran,
    /// not that it met its acceptance criteria.
    Completed,
    /// Completion with recorded verification evidence behind it.
    Verified,
    Failed,
    Cancelled,
}

/// Where one try of a task ended.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    /// Admitted and recorded, but its worker has not reported work yet. A
    /// start that returns before the child runs has to be distinguishable
    /// from one whose child is already spending the budget.
    Starting,
    Running,
    /// Running, but parked on something outside itself — a dependency, an
    /// unanswered question to its parent. Still leased, still cancellable.
    Waiting,
    /// Cancellation was requested and the holder has not acknowledged it.
    /// Kept as its own state so a cancel that never lands is visible rather
    /// than looking like a clean stop.
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    /// The lease ran out, or the holder was found gone. The attempt keeps its
    /// evidence, but it can no longer commit a result.
    Expired,
    /// A retry replaced this attempt. Its evidence stays readable; its result
    /// can no longer satisfy the task.
    Superseded,
}

impl AttemptState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Expired | Self::Superseded
        )
    }

    /// Whether the holder may still commit a result against this attempt.
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Waiting | Self::Cancelling
        )
    }
}

/// Whether a failed attempt may be tried again without asking anyone.
///
/// The holder classifies its own failure because only it knows what actually
/// happened: a stream that dropped mid-token is not the same event as a denial,
/// and a write whose outcome is unknown is not the same as one that never ran.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    /// A transient fault: the same request run again is the same request.
    Retryable,
    /// A denial, an invalid request, or anything a repeat would only repeat.
    #[default]
    NotRetryable,
    /// The effect may or may not have landed. Never retried automatically,
    /// because a retry would be a second write nobody asked for.
    UnknownOutcome,
}

/// Which model the attempt was routed to, recorded so a replayed attempt is
/// explicable without the configuration that happened to be loaded later.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDecision {
    pub profile: String,
    pub model: String,
}

/// What a caller supplies to start one try of a task.
#[derive(Clone, Debug)]
pub struct AttemptRequest {
    pub role: String,
    pub assignee: AgentId,
    pub model: Option<ModelDecision>,
    /// The workspace revision the attempt starts from. Evidence produced
    /// against a different revision is stale, which is a question only a
    /// recorded base can answer.
    pub base_revision: Option<WorkspaceVersion>,
    pub started_at_ms: u64,
    pub lease_expires_at_ms: u64,
}

/// One durable try of a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskAttempt {
    pub schema: u32,
    pub id: AttemptId,
    pub task: TaskId,
    pub parent_task: Option<TaskId>,
    pub parent_attempt: Option<AttemptId>,
    /// The attempt this one retries, so retry lineage survives a restart.
    pub retry_of: Option<AttemptId>,
    pub role: String,
    pub assignee: AgentId,
    pub model: Option<ModelDecision>,
    pub workspace: WorkspaceRequirement,
    pub base_revision: Option<WorkspaceVersion>,
    /// The authority the task held when the attempt started, not whatever the
    /// process holds now: a recovered attempt must not be explicable by a
    /// policy that was loaded after it ran.
    pub authority: Vec<CapabilityGrant>,
    pub required_output: String,
    pub reserved: Budget,
    pub used: Budget,
    /// Incremented on every start, so a result from a superseded lease can be
    /// recognised and refused rather than overwriting the retry that replaced it.
    pub lease_epoch: u64,
    pub lease_expires_at_ms: u64,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub state: AttemptState,
    pub terminal_reason: Option<String>,
    /// How its holder classified the failure. Meaningless while the attempt
    /// is live and for one that completed.
    #[serde(default)]
    pub retryable: Retryability,
    /// What the attempt produced, as the caller recorded it.
    pub result: Option<Value>,
    /// Artifact ids and other evidence references produced by this attempt.
    pub evidence: Vec<String>,
}

/// How one try of a task ended, as its holder reports it.
#[derive(Clone, Debug)]
pub struct AttemptOutcome {
    pub state: AttemptState,
    pub used: Budget,
    pub reason: Option<String>,
    pub retryable: Retryability,
    pub result: Option<Value>,
    pub evidence: Vec<String>,
    pub ended_at_ms: u64,
}

impl AttemptOutcome {
    pub fn completed(used: Budget, result: Value, ended_at_ms: u64) -> Self {
        Self {
            state: AttemptState::Completed,
            used,
            reason: None,
            retryable: Retryability::NotRetryable,
            result: Some(result),
            evidence: Vec::new(),
            ended_at_ms,
        }
    }

    /// A failure nobody should repeat on its own. `retryable` opts back in.
    pub fn failed(used: Budget, reason: impl Into<String>, ended_at_ms: u64) -> Self {
        Self {
            state: AttemptState::Failed,
            used,
            reason: Some(reason.into()),
            retryable: Retryability::NotRetryable,
            result: None,
            evidence: Vec::new(),
            ended_at_ms,
        }
    }

    #[must_use]
    pub const fn retryable(mut self, retryable: Retryability) -> Self {
        self.retryable = retryable;
        self
    }
}

/// Durable link from a scheduled task to the scratch plan's commitment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanTask {
    /// Planning task of origin, not a budget-reserving parent.
    pub root: Option<TaskId>,
    pub step_id: String,
    pub todo_id: String,
}

/// Graph-owned bookkeeping; callers may provide a parent or a committed
/// plan link, while the graph owns attempt and budget accounting.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskRuntime {
    pub parent: Option<TaskId>,
    /// An absent link means this is an ordinary task, including old events.
    #[serde(default)]
    pub plan: Option<PlanTask>,
    /// Budget this task's own work has settled, in the order it was recorded.
    pub used: Budget,
    /// Budget promised to children that have not settled yet. Reserved
    /// capacity is unavailable to this task and to any further child.
    pub reserved: Budget,
    pub attempts: Vec<AttemptId>,
    pub current_attempt: Option<AttemptId>,
    pub lease_epoch: u64,
    /// Whether this task's reservation has been returned to its parent.
    pub settled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub goal: String,
    pub dependencies: Vec<TaskId>,
    pub assignee: Option<AgentId>,
    pub required_output: String,
    pub workspace: WorkspaceRequirement,
    /// The whole allowance this task may spend, on itself and its children.
    pub budget: Budget,
    pub authority: Vec<CapabilityGrant>,
    pub state: TaskState,
    pub lease_expires_at_ms: Option<u64>,
    /// Defaulted so a stream written before attempts existed still replays.
    #[serde(default)]
    pub runtime: TaskRuntime,
}

impl TaskNode {
    /// What is left to spend: the allowance, minus what this task has used,
    /// minus what is promised to children that have not settled.
    pub fn available(&self) -> Budget {
        self.budget
            .saturating_sub(self.runtime.used)
            .saturating_sub(self.runtime.reserved)
    }
}

/// What a task's `required_output` demands of a result.
///
/// A task states its required output as text, and text is what most of them
/// want. When that text is a JSON object schema, the object shape in it is
/// checked instead, so a child cannot answer a structured request with prose.
///
/// ponytail: presence of `required` keys and `type: object`, not full JSON
/// Schema. A real validator is a dependency; add one when a task actually
/// needs `oneOf`, formats, or nested constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResultContract {
    Text,
    Object { required: Vec<String> },
}

impl ResultContract {
    /// Read the contract out of a task's `required_output`.
    pub fn parse(required_output: &str) -> Self {
        let Ok(schema) = serde_json::from_str::<Value>(required_output) else {
            return Self::Text;
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Self::Text;
        }
        Self::Object {
            required: schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Why this result does not meet the contract, if it does not.
    pub fn violation(&self, result: Option<&Value>) -> Option<String> {
        let Some(result) = result else {
            return Some("the attempt completed without a result".into());
        };
        match self {
            Self::Text => match result {
                Value::Null => Some("the attempt completed without a result".into()),
                Value::String(text) if text.trim().is_empty() => {
                    Some("the attempt completed with an empty result".into())
                }
                _ => None,
            },
            Self::Object { required } => {
                let Some(object) = result.as_object() else {
                    return Some(
                        "the required output is an object and the result is not one".into(),
                    );
                };
                let missing: Vec<&str> = required
                    .iter()
                    .filter(|name| !object.contains_key(*name))
                    .map(String::as_str)
                    .collect();
                (!missing.is_empty())
                    .then(|| format!("the result is missing {}", missing.join(", ")))
            }
        }
    }
}

/// What one task says to another, durably.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A narrowing of what was already asked. Never a new grant.
    Instruction,
    Question,
    Answer,
    Progress,
    PartialResult,
    ReviewerFeedback,
    Cancellation,
    DependencyWakeup,
}

/// One durable message between tasks.
///
/// Messages carry text and structure, never authority: a child's grants are
/// fixed when it is created, and nothing delivered to it afterwards can add to
/// them. That is why this record has no capability field to widen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskMessage {
    pub id: MessageId,
    pub from: TaskId,
    pub to: TaskId,
    pub kind: MessageKind,
    pub body: Value,
    /// The message this one answers or follows from.
    pub causation: Option<MessageId>,
    pub sent_at_ms: u64,
    /// When the recipient took it off its inbox. Delivery is recorded rather
    /// than assumed, so a message lost to a crash is redelivered rather than
    /// silently dropped.
    pub delivered_at_ms: Option<u64>,
}

/// How an isolated view was made.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationBackend {
    GitWorktree,
    CopiedSnapshot,
}

/// What was done about uncommitted work in the source when a view was cut.
///
/// Never absent: a writer that silently started from a tree missing the
/// operator's uncommitted changes would produce a diff against a base nobody
/// can reconstruct.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyDisposition {
    /// The source had nothing uncommitted.
    Clean,
    /// The source was dirty and the assignment was refused.
    Refused,
    /// The source was dirty and its changes were captured as an artifact the
    /// view starts from.
    CapturedPatch { artifact: String },
}

/// One task's claim on one filesystem view.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceAssignment {
    pub id: AssignmentId,
    pub task: TaskId,
    pub attempt: AttemptId,
    pub owner: AgentId,
    /// What repository this is, independent of where it happens to be
    /// checked out — the first commit for a Git repository, so two clones of
    /// one project are not mistaken for two projects.
    pub repository: String,
    /// The canonical checkout the view was cut from.
    pub source: String,
    pub base_revision: String,
    pub view: String,
    pub backend: IsolationBackend,
    pub mutable: bool,
    pub lease_epoch: u64,
    pub expires_at_ms: u64,
    pub dirty: DirtyDisposition,
    pub released: bool,
}

/// What a writer produced, as the thing an integrator reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriterResult {
    pub assignment: AssignmentId,
    pub base_revision: String,
    /// The revision the writer ended on, when its view was a Git worktree.
    pub head_revision: Option<String>,
    /// A patch artifact, for a view with no revisions of its own.
    pub patch_artifact: Option<String>,
    pub changed_files: Vec<String>,
    /// Artifact ids for the checks the writer ran in its own view.
    pub validation: Vec<String>,
    /// What the writer knows it did not settle. Stated rather than omitted:
    /// an integrator has to be able to refuse work that says it is unfinished.
    pub unresolved: Vec<String>,
}

/// What decides whether one acceptance criterion is met.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verifier {
    /// A recorded check. The digest is of the command text, so a pass from a
    /// different command cannot be offered in its place.
    Command { digest: StateVersion },
    /// A reviewer's verdict. Attributed to whoever gave it and never
    /// described as deterministic, because it is not.
    Review,
    /// Only a person can say. Same treatment as a review, and named
    /// separately so a report can distinguish "nobody has looked" from
    /// "nobody has run it".
    Human,
    /// Nothing available can decide it. Stated rather than left to look
    /// unchecked: a criterion nobody can verify is a fact about the
    /// criterion, not a gap in the work.
    Unverifiable { why: String },
}

/// One thing that has to hold before work may be called verified.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceCriterion {
    pub id: CriterionId,
    pub task: TaskId,
    pub statement: String,
    pub verifier: Verifier,
    /// A criterion that is not required may be unmet without blocking.
    pub required: bool,
    /// How old its evidence may be. `None` means only the workspace revision
    /// decides, which is the usual case.
    pub freshness_ms: Option<u64>,
    /// Set when the criterion stops applying — a check for a component the
    /// work removed. Applicability is recorded, never assumed.
    pub inapplicable: Option<String>,
}

/// A verdict a person or a reviewing agent gave.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Judgment {
    pub criterion: CriterionId,
    pub actor: Principal,
    pub met: bool,
    pub note: String,
    pub workspace_revision: Option<WorkspaceVersion>,
    pub recorded_at_ms: u64,
}

pub struct ChildCapabilityRequest {
    pub parent_grant: usize,
    pub action: CapabilityAction,
    pub scope: ResourceScope,
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinPolicy {
    All,
    Any,
    Quorum(usize),
}

impl JoinPolicy {
    pub fn satisfied(self, states: impl IntoIterator<Item = TaskState>) -> bool {
        let states: Vec<_> = states.into_iter().collect();
        let completed = states
            .iter()
            .filter(|state| matches!(**state, TaskState::Completed | TaskState::Verified))
            .count();
        match self {
            Self::All => !states.is_empty() && completed == states.len(),
            Self::Any => completed > 0,
            Self::Quorum(required) => required > 0 && completed >= required,
        }
    }
}

pub struct TaskGraph {
    store: Arc<dyn EventStore>,
    session: SessionId,
    actor: Principal,
    version: StreamVersion,
    last_event: Option<crate::domain::EventId>,
    nodes: BTreeMap<TaskId, TaskNode>,
    /// Reverse edges maintained during replay, so settling a failed branch
    /// only visits its descendants rather than searching the whole graph.
    dependents: BTreeMap<TaskId, BTreeSet<TaskId>>,
    /// Approved steps and the optional operator constraint from their event.
    approved_plan: BTreeMap<TaskId, Option<String>>,
    /// A root's pre-commit plan, recorded before either projection is changed.
    plan_intents: BTreeMap<TaskId, Value>,
    plan_todos: BTreeMap<(TaskId, String), String>,
    todo_ids: BTreeSet<String>,
    scripted_plan_roots: BTreeSet<TaskId>,
    block_seeds: BTreeSet<TaskId>,
    reconciling: bool,
    attempts: BTreeMap<AttemptId, TaskAttempt>,
    messages: BTreeMap<MessageId, TaskMessage>,
    /// Undelivered message ids per recipient, oldest first. Delivery order is
    /// the order they were appended to the one stream, so two senders cannot
    /// disagree about what the recipient saw first.
    inbox: BTreeMap<TaskId, Vec<MessageId>>,
    /// The tail of what each attempt recorded.
    ///
    /// ponytail: the projection keeps the last [`MAX_TRACE_ENTRIES`], not all
    /// of them — the stream has the rest. Page it from the store if a reader
    /// ever needs more than the tail.
    traces: BTreeMap<AttemptId, Vec<Value>>,
    paused: BTreeSet<AttemptId>,
    assignments: BTreeMap<AssignmentId, WorkspaceAssignment>,
    writer_results: BTreeMap<AssignmentId, WriterResult>,
    criteria: BTreeMap<CriterionId, AcceptanceCriterion>,
    judgments: BTreeMap<CriterionId, Vec<Judgment>>,
}

impl TaskGraph {
    /// Rebuild the graph from its session's stream, a page at a time.
    ///
    /// Paged because a long session's stream is not a thing to read at once,
    /// and because worker recovery wants the task projection rather than the
    /// conversation: this reads the same events the transcript does, but keeps
    /// only what a task needs.
    pub fn new(
        store: Arc<dyn EventStore>,
        session: SessionId,
        actor: Principal,
    ) -> Result<Self, GraphError> {
        let mut graph = Self {
            store,
            session,
            actor,
            version: StreamVersion(0),
            last_event: None,
            nodes: BTreeMap::new(),
            dependents: BTreeMap::new(),
            block_seeds: BTreeSet::new(),
            plan_intents: BTreeMap::new(),
            plan_todos: BTreeMap::new(),
            todo_ids: BTreeSet::new(),
            approved_plan: BTreeMap::new(),
            scripted_plan_roots: BTreeSet::new(),
            reconciling: false,
            attempts: BTreeMap::new(),
            messages: BTreeMap::new(),
            inbox: BTreeMap::new(),
            traces: BTreeMap::new(),
            paused: BTreeSet::new(),
            assignments: BTreeMap::new(),
            writer_results: BTreeMap::new(),
            criteria: BTreeMap::new(),
            judgments: BTreeMap::new(),
        };
        graph.catch_up()?;
        Ok(graph)
    }

    /// A second view of the same session, for a worker on another thread.
    ///
    /// Not a copy of this projection: it replays the stream for itself, so a
    /// worker that appends while the parent is busy does not have to reach
    /// into the parent's state to be seen. Both converge because both read
    /// the one stream and append against its version.
    pub fn fork(&self, actor: Principal) -> Result<Self, GraphError> {
        Self::new(Arc::clone(&self.store), self.session, actor)
    }

    pub fn add(&mut self, node: TaskNode) -> Result<(), GraphError> {
        let node = TaskNode {
            runtime: TaskRuntime {
                parent: node.runtime.parent,
                plan: node.runtime.plan,
                ..TaskRuntime::default()
            },
            ..node
        };
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::Duplicate(node.id));
        }
        if node.dependencies.contains(&node.id) || self.would_cycle(node.id, &node.dependencies) {
            self.commit("task.cycle_detected", |_| Ok(json!({"task_id": node.id})))?;
            return Err(GraphError::Cycle(node.id));
        }
        self.commit("task.created", |graph| {
            if graph.nodes.contains_key(&node.id) {
                return Err(GraphError::Duplicate(node.id));
            }
            if graph.would_cycle(node.id, &node.dependencies) {
                return Err(GraphError::Cycle(node.id));
            }
            // Re-checked here rather than only above: between building this
            // event and appending it, another writer may have spent the
            // parent's remaining capacity.
            if let Some(parent) = node.runtime.parent {
                let parent = graph
                    .nodes
                    .get(&parent)
                    .ok_or(GraphError::Unknown(parent))?;
                if !node.budget.fits_within(parent.available()) {
                    return Err(GraphError::BudgetExpansion);
                }
            }
            Ok(json!({"node": &node}))
        })
    }

    /// Persist the proposed commitment before creating its tasks and TODOs.
    /// Repeated calls are read-only; a later proposal may only append steps.
    pub fn record_plan_intent(&mut self, root: TaskId, steps: &[Value]) -> Result<(), GraphError> {
        self.commit_if("task.plan_intent", |graph| {
            if !graph.nodes.contains_key(&root) {
                return Err(GraphError::Unknown(root));
            }
            let proposed = Value::Array(steps.to_vec());
            if let Some(previous) = graph.plan_intents.get(&root) {
                let old = previous
                    .as_array()
                    .ok_or_else(|| GraphError::InvalidEvent("invalid prior plan intent".into()))?;
                if old.len() > steps.len() || old.as_slice() != &steps[..old.len()] {
                    return Err(GraphError::InvalidEvent(
                        "committed plan steps cannot be revised; append new steps instead".into(),
                    ));
                }
                if old.len() == steps.len() {
                    return Ok(None);
                }
            }
            Ok(Some(json!({"root": root, "steps": proposed})))
        })?;
        Ok(())
    }

    pub fn plan_intent(&self, root: TaskId) -> Option<&Value> {
        self.plan_intents.get(&root)
    }
    /// Roots whose durable plan commitments can be recovered without scratch.
    pub fn plan_intent_roots(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.plan_intents.keys().copied()
    }

    /// Append a plan-linked TODO with its reserved ID, rechecking ID ownership
    /// after each stream conflict. The ordinary TODO projection reads the same
    /// `todo.created` event; the linkage is additional event metadata.
    pub fn record_plan_todo(
        &mut self,
        root: TaskId,
        step_id: &str,
        item: &TodoItem,
    ) -> Result<(), GraphError> {
        self.commit_if("todo.created", |graph| {
            if let Some(id) = graph.plan_todos.get(&(root, step_id.to_owned())) {
                if id == &item.id {
                    return Ok(None);
                }
                return Err(GraphError::InvalidEvent(
                    "plan step already has a different TODO".into(),
                ));
            }
            let reserved = graph
                .plan_intents
                .get(&root)
                .and_then(Value::as_array)
                .and_then(|steps| {
                    steps
                        .iter()
                        .find(|step| step.get("id").and_then(Value::as_str) == Some(step_id))
                })
                .and_then(|step| step["committed_as"].as_str());
            if reserved != Some(item.id.as_str()) || graph.todo_ids.contains(&item.id) {
                return Err(GraphError::InvalidEvent(format!(
                    "TODO {} is not reserved by this plan or was claimed; inspect session history",
                    item.id
                )));
            }
            Ok(Some(
                json!({"item": item, "plan_root": root, "step_id": step_id}),
            ))
        })?;
        Ok(())
    }

    pub fn plan_todo_id(&self, root: TaskId, step_id: &str) -> Option<&str> {
        self.plan_todos
            .get(&(root, step_id.to_owned()))
            .map(String::as_str)
    }

    /// Scripted execution consent, durable independently of later step
    /// approval so a crash never turns an interactive draft into auto-run.
    pub fn mark_scripted_plan(&mut self, root: TaskId) -> Result<(), GraphError> {
        self.commit_if("task.scripted_plan_authorized", |graph| {
            if !graph.nodes.contains_key(&root) {
                return Err(GraphError::Unknown(root));
            }
            if graph.scripted_plan_roots.contains(&root) {
                return Ok(None);
            }
            Ok(Some(json!({"root": root})))
        })?;
        Ok(())
    }

    pub fn is_scripted_plan_authorized(&self, root: TaskId) -> bool {
        self.scripted_plan_roots.contains(&root)
    }

    /// Approve committed steps in one event, rechecking after concurrent writes.
    /// A retry must supply the same constraint; approval cannot revise it.
    pub fn approve_plan(
        &mut self,
        task_ids: &[TaskId],
        note: Option<&str>,
    ) -> Result<(), GraphError> {
        self.commit_if("task.plan_approved", |graph| {
            if task_ids.is_empty() {
                return Err(GraphError::InvalidEvent(
                    "approval has no plan tasks".into(),
                ));
            }
            let mut pending = BTreeSet::new();
            for &id in task_ids {
                graph.validate_plan_task(id)?;
                match graph.approved_plan.get(&id) {
                    Some(recorded) if recorded.as_deref() != note => {
                        return Err(GraphError::PlanApprovalConflict(id));
                    }
                    Some(_) => {}
                    None => {
                        pending.insert(id);
                    }
                }
            }
            if pending.is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({"task_ids": pending, "note": note})))
        })?;
        Ok(())
    }

    fn validate_plan_task(&self, id: TaskId) -> Result<(), GraphError> {
        let node = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
        let plan = node
            .runtime
            .plan
            .as_ref()
            .ok_or(GraphError::NotAPlanTask(id))?;
        let root = plan
            .root
            .filter(|root| *root != id)
            .and_then(|root| self.nodes.get(&root));
        if root.is_none_or(|root| !node.budget.fits_within(root.budget))
            || plan.step_id.is_empty()
            || plan.todo_id.is_empty()
            || node.budget.tokens == 0
            || node.workspace != WorkspaceRequirement::IsolatedWriter
        {
            return Err(GraphError::NotAPlanTask(id));
        }
        Ok(())
    }

    pub fn is_plan_approved(&self, id: TaskId) -> bool {
        self.approved_plan.contains_key(&id)
    }

    pub fn plan_approval_note(&self, id: TaskId) -> Option<&str> {
        self.approved_plan.get(&id).and_then(Option::as_deref)
    }

    pub fn approved_plan_tasks(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.approved_plan.keys().copied()
    }

    /// Record what a task holds, so it has something to delegate from.
    ///
    /// Authority is minted by policy for a caller, not by the graph — but a
    /// parent cannot attenuate a grant the graph has never seen, and a child's
    /// authority has to be explicable from the record rather than from whatever
    /// the process happened to be holding. So the grants are written down
    /// against the task, once, and every child is derived from what is written.
    ///
    /// Refuses to widen: a task's authority is set while it is the only thing
    /// that could have used it, and replacing it later would let a parent grant
    /// a child more than it had when its own children were checked.
    pub fn authorize(
        &mut self,
        id: TaskId,
        authority: Vec<CapabilityGrant>,
    ) -> Result<(), GraphError> {
        self.commit("task.authorized", |graph| {
            let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
            if !node.authority.is_empty() {
                return Err(GraphError::AlreadyAuthorized(id));
            }
            Ok(json!({"task_id": id, "authority": &authority}))
        })
    }

    /// Re-derive a recovered task's authority against the policy in force now.
    ///
    /// A resumed task has to be explicable by what the operator allows today.
    /// So each recorded grant is kept only while a current grant for the same
    /// action still exists, narrowed by that grant's scope, expiry, and
    /// delegation depth; a recorded grant with no counterpart is dropped.
    ///
    /// Only ever narrows. A policy loaded after the task ran cannot hand it
    /// authority it did not hold — that would make a restart a way to acquire
    /// permissions, and the grants its children were checked against would no
    /// longer bound them.
    pub fn narrow_authority(
        &mut self,
        id: TaskId,
        current: &[CapabilityGrant],
    ) -> Result<Vec<CapabilityGrant>, GraphError> {
        let narrowed: Vec<CapabilityGrant> = self
            .nodes
            .get(&id)
            .ok_or(GraphError::Unknown(id))?
            .authority
            .iter()
            .filter_map(|held| {
                let now = current.iter().find(|grant| grant.action == held.action)?;
                Some(CapabilityGrant {
                    scope: held.scope.narrow(&now.scope),
                    expires_at_ms: match (held.expires_at_ms, now.expires_at_ms) {
                        (Some(held), Some(now)) => Some(held.min(now)),
                        (held, now) => held.or(now),
                    },
                    delegation_depth: held.delegation_depth.min(now.delegation_depth),
                    ..held.clone()
                })
            })
            .collect();
        self.commit("task.authority_narrowed", |_| {
            Ok(json!({"task_id": id, "authority": &narrowed}))
        })?;
        Ok(narrowed)
    }

    /// Create a child, reserving its whole budget from the parent.
    ///
    /// Reservation and creation are one event because they are one decision:
    /// a child that exists without its budget held against the parent would
    /// let the next sibling be promised the same capacity.
    pub fn add_child(
        &mut self,
        parent: TaskId,
        mut child: TaskNode,
        requests: Vec<ChildCapabilityRequest>,
    ) -> Result<(), GraphError> {
        let parent_node = self.nodes.get(&parent).ok_or(GraphError::Unknown(parent))?;
        if !child.budget.fits_within(parent_node.available()) {
            return Err(GraphError::BudgetExpansion);
        }
        let assignee = child.assignee.ok_or(GraphError::MissingAssignee)?;
        child.authority = requests
            .into_iter()
            .map(|request| {
                parent_node
                    .authority
                    .get(request.parent_grant)
                    .ok_or(GraphError::MissingParentGrant)?
                    .attenuate(
                        Principal::Agent(assignee),
                        request.action,
                        &request.scope,
                        request.expires_at_ms,
                    )
                    .map_err(GraphError::Attenuation)
            })
            .collect::<Result<_, _>>()?;
        child.runtime.parent = Some(parent);
        self.add(child)
    }

    pub fn ready(&mut self) -> Result<Vec<TaskId>, GraphError> {
        self.refresh()?;
        self.reconcile_blocked()?;
        let candidates: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| self.is_ready(node).then_some(*id))
            .collect();
        let mut ready = Vec::new();
        for id in candidates {
            if self.commit_if("task.transitioned", |graph| {
                if !graph.is_ready(graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?) {
                    return Ok(None);
                }
                Ok(Some(json!({"task_id": id, "from": TaskState::Pending, "to": TaskState::Ready, "evidence": null})))
            })? {
                ready.push(id);
            }
        }
        Ok(ready)
    }

    /// Start one try of a task, returning the attempt it can be fenced by.
    pub fn start_attempt(
        &mut self,
        id: TaskId,
        request: &AttemptRequest,
    ) -> Result<AttemptId, GraphError> {
        let attempt_id = AttemptId::new();
        self.commit("task.attempt_started", |graph| {
            let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
            if node.state != TaskState::Ready {
                return Err(GraphError::InvalidTransition(
                    node.state,
                    TaskState::Running,
                ));
            }
            if !graph.dependencies_completed(node) {
                return Err(GraphError::InvalidTransition(
                    node.state,
                    TaskState::Running,
                ));
            }
            if node.runtime.plan.is_some() && !graph.is_plan_approved(id) {
                return Err(GraphError::PlanNotApproved(id));
            }
            let parent_attempt = node
                .runtime
                .parent
                .and_then(|parent| graph.nodes.get(&parent))
                .and_then(|parent| parent.runtime.current_attempt);
            let attempt = TaskAttempt {
                schema: ATTEMPT_SCHEMA_VERSION,
                id: attempt_id,
                task: id,
                parent_task: node.runtime.parent,
                parent_attempt,
                retry_of: node.runtime.attempts.last().copied(),
                role: request.role.clone(),
                assignee: request.assignee,
                model: request.model.clone(),
                workspace: node.workspace,
                base_revision: request.base_revision,
                authority: node.authority.clone(),
                required_output: node.required_output.clone(),
                reserved: node.available(),
                used: Budget::default(),
                lease_epoch: node.runtime.lease_epoch.saturating_add(1),
                lease_expires_at_ms: request.lease_expires_at_ms,
                started_at_ms: request.started_at_ms,
                ended_at_ms: None,
                // Admitted, not yet working. A worker reports `Running` once
                // it has actually picked the attempt up, so a start that
                // returns before its child runs is visible as exactly that.
                state: AttemptState::Starting,
                terminal_reason: None,
                retryable: Retryability::default(),
                result: None,
                evidence: Vec::new(),
            };
            Ok(json!({"attempt": &attempt}))
        })?;
        Ok(attempt_id)
    }

    /// Start an attempt with nothing recorded about it but its holder.
    ///
    /// The shape callers had before attempts were durable, kept because most
    /// of them have nothing more to say than who is working and until when.
    pub fn lease(
        &mut self,
        id: TaskId,
        agent: AgentId,
        expires_at_ms: u64,
    ) -> Result<AttemptId, GraphError> {
        self.start_attempt(
            id,
            &AttemptRequest {
                role: String::new(),
                assignee: agent,
                model: None,
                base_revision: None,
                started_at_ms: crate::artifact::unix_time_ms(),
                lease_expires_at_ms: expires_at_ms,
            },
        )
    }

    /// Move a live attempt between its non-terminal states.
    ///
    /// Terminal states are not reachable from here: ending an attempt settles
    /// budget and a task state with it, which is [`finish_attempt`]'s job.
    ///
    /// [`finish_attempt`]: Self::finish_attempt
    pub fn advance_attempt(
        &mut self,
        attempt: AttemptId,
        target: AttemptState,
    ) -> Result<(), GraphError> {
        if !target.is_live() {
            return Err(GraphError::NotLive(target));
        }
        self.commit("task.attempt_state_changed", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if !record.state.is_live() {
                return Err(GraphError::FencedAttempt(attempt));
            }
            if graph
                .nodes
                .get(&record.task)
                .is_some_and(|node| node.runtime.plan.is_some())
                && !graph.is_plan_approved(record.task)
            {
                return Err(GraphError::PlanNotApproved(record.task));
            }
            // Cancellation is one-way: a holder asked to stop does not get to
            // go back to running and keep spending.
            if record.state == AttemptState::Cancelling && target != AttemptState::Cancelling {
                return Err(GraphError::Cancelling(attempt));
            }
            Ok(json!({"attempt_id": attempt, "task_id": record.task, "state": target}))
        })
    }

    /// Ask a running attempt to stop, without ending it here.
    ///
    /// Recorded before the holder acknowledges, because the request is the
    /// durable part: a process that dies between the ask and the stop must
    /// come back knowing the attempt was told to end, not resume it.
    pub fn request_cancel(
        &mut self,
        attempt: AttemptId,
        reason: impl Into<String>,
    ) -> Result<(), GraphError> {
        let reason = reason.into();
        self.commit("task.attempt_cancel_requested", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if !record.state.is_live() {
                return Err(GraphError::FencedAttempt(attempt));
            }
            Ok(json!({
                "attempt_id": attempt,
                "task_id": record.task,
                "reason": reason,
            }))
        })
    }

    /// Ask a live attempt to pause at its next model/tool boundary.
    pub fn request_pause(&mut self, attempt: AttemptId) -> Result<(), GraphError> {
        self.commit("task.attempt_pause_requested", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if !record.state.is_live() || record.state == AttemptState::Cancelling {
                return Err(GraphError::FencedAttempt(attempt));
            }
            Ok(json!({"attempt_id": attempt, "task_id": record.task}))
        })
    }

    /// Release a cooperatively paused attempt.
    pub fn resume(&mut self, attempt: AttemptId) -> Result<(), GraphError> {
        self.commit("task.attempt_resumed", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if !record.state.is_live() || !graph.paused.contains(&attempt) {
                return Err(GraphError::FencedAttempt(attempt));
            }
            if graph
                .nodes
                .get(&record.task)
                .is_some_and(|node| node.runtime.plan.is_some())
                && !graph.is_plan_approved(record.task)
            {
                return Err(GraphError::PlanNotApproved(record.task));
            }
            Ok(json!({"attempt_id": attempt, "task_id": record.task}))
        })
    }

    pub fn is_paused(&self, attempt: AttemptId) -> bool {
        self.paused.contains(&attempt)
    }

    /// Fold canonical events appended by another graph view.
    pub fn refresh(&mut self) -> Result<(), GraphError> {
        self.catch_up()
    }

    /// Retire the task's current attempt and start a fresh one.
    ///
    /// Refuses unless the attempt it replaces classified itself retryable: a
    /// denial repeated is still a denial, and an effect whose outcome is
    /// unknown must not be applied a second time by a scheduler's initiative.
    pub fn retry(
        &mut self,
        id: TaskId,
        request: &AttemptRequest,
        max_attempts: usize,
    ) -> Result<AttemptId, GraphError> {
        let node = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
        if node.runtime.plan.is_some() && !self.is_plan_approved(id) {
            return Err(GraphError::PlanNotApproved(id));
        }
        if node.runtime.attempts.len() >= max_attempts {
            return Err(GraphError::RetriesExhausted(id));
        }
        let last = node
            .runtime
            .attempts
            .last()
            .and_then(|attempt| self.attempts.get(attempt))
            .ok_or(GraphError::NothingToRetry(id))?;
        if last.state.is_live() {
            return Err(GraphError::StillRunning(last.id));
        }
        if last.retryable != Retryability::Retryable {
            return Err(GraphError::NotRetryable(last.id, last.retryable));
        }
        if node.available() == Budget::default() {
            return Err(GraphError::BudgetExhausted(PartialEvidence {
                reason: "budget_exhausted".into(),
                remaining: Budget::default(),
            }));
        }
        let superseded = last.id;
        self.commit("task.attempt_superseded", |graph| {
            let record = graph
                .attempts
                .get(&superseded)
                .ok_or(GraphError::UnknownAttempt(superseded))?;
            Ok(json!({"attempt_id": superseded, "task_id": record.task}))
        })?;
        self.transition(id, TaskState::Ready, None)?;
        self.start_attempt(id, request)
    }

    /// Commit one acceptance criterion against a task.
    ///
    /// Written down before the work is judged against it, so a criterion
    /// cannot be invented to fit what happened to pass.
    pub fn declare_criterion(
        &mut self,
        criterion: AcceptanceCriterion,
    ) -> Result<CriterionId, GraphError> {
        let id = criterion.id;
        self.commit("task.criterion_declared", |graph| {
            if !graph.nodes.contains_key(&criterion.task) {
                return Err(GraphError::Unknown(criterion.task));
            }
            if graph.criteria.contains_key(&id) {
                return Err(GraphError::DuplicateCriterion(id));
            }
            Ok(json!({"criterion": &criterion}))
        })?;
        Ok(id)
    }

    /// Record a reviewer's or a person's verdict on one criterion.
    ///
    /// Attributed, and never converted into a deterministic result: the
    /// proof reports it as a judgment by whoever gave it.
    pub fn judge(&mut self, judgment: Judgment) -> Result<(), GraphError> {
        self.commit("task.criterion_judged", |graph| {
            let criterion = graph
                .criteria
                .get(&judgment.criterion)
                .ok_or(GraphError::UnknownCriterion(judgment.criterion))?;
            if !matches!(criterion.verifier, Verifier::Review | Verifier::Human) {
                return Err(GraphError::NotAJudgment(judgment.criterion));
            }
            Ok(json!({"judgment": &judgment}))
        })
    }

    /// Say that a criterion no longer applies, and why.
    pub fn retire_criterion(
        &mut self,
        id: CriterionId,
        why: impl Into<String>,
    ) -> Result<(), GraphError> {
        let why = why.into();
        self.commit("task.criterion_retired", |graph| {
            if !graph.criteria.contains_key(&id) {
                return Err(GraphError::UnknownCriterion(id));
            }
            Ok(json!({"criterion_id": id, "why": why}))
        })
    }

    pub fn criterion(&self, id: CriterionId) -> Option<&AcceptanceCriterion> {
        self.criteria.get(&id)
    }

    /// Every criterion committed against one task, in the order declared.
    pub fn criteria_of(&self, task: TaskId) -> Vec<&AcceptanceCriterion> {
        self.criteria
            .values()
            .filter(|criterion| criterion.task == task)
            .collect()
    }

    /// Verdicts given on one criterion, oldest first.
    pub fn judgments_of(&self, id: CriterionId) -> &[Judgment] {
        self.judgments
            .get(&id)
            .map_or(&[], |verdicts| verdicts.as_slice())
    }

    /// Claim a filesystem view for one attempt.
    ///
    /// Three things are refused here rather than left to whoever cut the
    /// directory, because they are the invariants the phase exists for:
    ///
    /// - a mutable view has exactly one live writer, so two agents cannot be
    ///   told they own the same tree;
    /// - a mutable view is never the canonical source, so writer authority
    ///   cannot reach the workspace an operator is looking at;
    /// - a refused dirty source produces no claim at all, so nothing runs
    ///   against a base that quietly dropped uncommitted work.
    pub fn assign_workspace(
        &mut self,
        assignment: WorkspaceAssignment,
    ) -> Result<AssignmentId, GraphError> {
        let id = assignment.id;
        if assignment.dirty == DirtyDisposition::Refused {
            return Err(GraphError::DirtySource(assignment.source));
        }
        if assignment.mutable && assignment.view == assignment.source {
            return Err(GraphError::WriterOnSource(assignment.source));
        }
        self.commit("workspace.assigned", |graph| {
            if !graph.attempts.contains_key(&assignment.attempt) {
                return Err(GraphError::UnknownAttempt(assignment.attempt));
            }
            if let Some(held) = graph
                .live_assignments()
                .find(|held| held.view == assignment.view && (held.mutable || assignment.mutable))
            {
                return Err(GraphError::ViewAlreadyOwned(held.owner, held.view.clone()));
            }
            Ok(json!({"assignment": &assignment}))
        })?;
        Ok(id)
    }

    /// Give a view back, so the next writer can have it.
    pub fn release_workspace(
        &mut self,
        id: AssignmentId,
        reason: impl Into<String>,
    ) -> Result<(), GraphError> {
        let reason = reason.into();
        self.commit("workspace.released", |graph| {
            let assignment = graph
                .assignments
                .get(&id)
                .ok_or(GraphError::UnknownAssignment(id))?;
            Ok(json!({
                "assignment_id": id,
                "task_id": assignment.task,
                "reason": reason,
            }))
        })
    }

    /// Record what a writer produced, against its assignment.
    pub fn record_writer_result(&mut self, result: &WriterResult) -> Result<(), GraphError> {
        self.commit("workspace.writer_result", |graph| {
            let assignment = graph
                .assignments
                .get(&result.assignment)
                .ok_or(GraphError::UnknownAssignment(result.assignment))?;
            if !assignment.mutable {
                return Err(GraphError::NotAWriter(result.assignment));
            }
            Ok(json!({
                "assignment_id": result.assignment,
                "task_id": assignment.task,
                "result": result,
            }))
        })
    }

    pub fn assignment(&self, id: AssignmentId) -> Option<&WorkspaceAssignment> {
        self.assignments.get(&id)
    }

    pub fn writer_result(&self, id: AssignmentId) -> Option<&WriterResult> {
        self.writer_results.get(&id)
    }

    /// Every claim that has not been given back, in creation order.
    pub fn live_assignments(&self) -> impl Iterator<Item = &WorkspaceAssignment> {
        self.assignments
            .values()
            .filter(|assignment| !assignment.released)
    }

    /// Every claim, including released views, in stable id order.
    pub fn assignments(&self) -> impl Iterator<Item = &WorkspaceAssignment> {
        self.assignments.values()
    }

    /// Claims whose owner is gone: the lease has passed, or the attempt that
    /// held it has ended without the view being given back.
    ///
    /// These are what a restarted process has to decide about — reuse, keep
    /// for inspection, or delete — and leaving them unnamed is how a machine
    /// fills up with worktrees nobody remembers cutting.
    pub fn abandoned_assignments(&self, now_ms: u64) -> Vec<&WorkspaceAssignment> {
        self.live_assignments()
            .filter(|assignment| {
                assignment.expires_at_ms <= now_ms
                    || self
                        .attempts
                        .get(&assignment.attempt)
                        .is_some_and(|attempt| attempt.state.is_terminal())
            })
            .collect()
    }

    /// Record one thing an attempt did, against the attempt.
    ///
    /// This is what makes a child inspectable after the fact: its prompts,
    /// decisions, and failures land in the same stream as everything else, so
    /// a restarted process can read what a child was doing without the child
    /// having a transcript of its own to lose.
    pub fn record(
        &mut self,
        attempt: AttemptId,
        kind: &str,
        data: Value,
    ) -> Result<(), GraphError> {
        self.commit("task.attempt_trace", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            Ok(json!({
                "attempt_id": attempt,
                "task_id": record.task,
                "kind": kind,
                "data": data,
            }))
        })
    }

    /// What an attempt recorded, oldest first.
    pub fn trace_of(&self, attempt: AttemptId) -> &[Value] {
        self.traces
            .get(&attempt)
            .map_or(&[], |entries| entries.as_slice())
    }

    /// Post a message to a task's inbox.
    pub fn send(
        &mut self,
        from: TaskId,
        to: TaskId,
        kind: MessageKind,
        body: Value,
        causation: Option<MessageId>,
        sent_at_ms: u64,
    ) -> Result<MessageId, GraphError> {
        let id = MessageId::new();
        self.commit("task.message_sent", |graph| {
            if !graph.nodes.contains_key(&from) {
                return Err(GraphError::Unknown(from));
            }
            let recipient = graph.nodes.get(&to).ok_or(GraphError::Unknown(to))?;
            if matches!(
                recipient.state,
                TaskState::Completed | TaskState::Verified | TaskState::Failed
            ) {
                return Err(GraphError::Ended(to, recipient.state));
            }
            // Backpressure: a sender that outruns a recipient is told so
            // rather than growing an inbox nobody drains.
            if graph.inbox.get(&to).map_or(0, Vec::len) >= MAX_INBOX {
                return Err(GraphError::InboxFull(to));
            }
            let message = TaskMessage {
                id,
                from,
                to,
                kind,
                body: body.clone(),
                causation,
                sent_at_ms,
                delivered_at_ms: None,
            };
            Ok(json!({"message": &message}))
        })?;
        Ok(id)
    }

    /// Take everything waiting for a task, marking it delivered.
    ///
    /// Delivery is committed before the caller acts on the messages, so a
    /// crash mid-handling redelivers nothing it already recorded as seen —
    /// and the record says what the recipient was given, which is the part an
    /// audit needs.
    pub fn deliver(&mut self, to: TaskId, now_ms: u64) -> Result<Vec<TaskMessage>, GraphError> {
        let waiting: Vec<MessageId> = self.inbox.get(&to).cloned().unwrap_or_default();
        if waiting.is_empty() {
            return Ok(Vec::new());
        }
        self.commit("task.messages_delivered", |graph| {
            let waiting: Vec<MessageId> = graph.inbox.get(&to).cloned().unwrap_or_default();
            Ok(json!({"task_id": to, "message_ids": waiting, "delivered_at_ms": now_ms}))
        })?;
        Ok(waiting
            .iter()
            .filter_map(|id| self.messages.get(id).cloned())
            .collect())
    }

    /// Messages a task has not taken yet, oldest first.
    pub fn inbox(&self, to: TaskId) -> Vec<&TaskMessage> {
        self.inbox
            .get(&to)
            .map(|ids| ids.iter().filter_map(|id| self.messages.get(id)).collect())
            .unwrap_or_default()
    }

    pub fn message(&self, id: MessageId) -> Option<&TaskMessage> {
        self.messages.get(&id)
    }

    /// Mark every task that can no longer become ready, including failures
    /// appended by another view since this one last read the stream.
    pub fn block_unreachable(&mut self) -> Result<Vec<TaskId>, GraphError> {
        self.catch_up()?;
        self.reconcile_blocked()
    }

    /// Close one attempt, settling what it spent.
    ///
    /// Refuses an attempt that is not the task's current one: a lease that
    /// expired and was retried has been superseded, and a late result from it
    /// would overwrite the try that replaced it with an answer about a
    /// workspace and a decision that no longer apply.
    pub fn finish_attempt(
        &mut self,
        attempt: AttemptId,
        outcome: &AttemptOutcome,
    ) -> Result<(), GraphError> {
        // `Expired` and `Superseded` are things done *to* an attempt by
        // recovery and retry; a holder reporting one would be describing a
        // decision it did not make.
        if !matches!(
            outcome.state,
            AttemptState::Completed | AttemptState::Failed | AttemptState::Cancelled
        ) {
            return Err(GraphError::NotTerminal(outcome.state));
        }
        self.commit("task.attempt_finished", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if !record.state.is_live() {
                return Err(GraphError::FencedAttempt(attempt));
            }
            let node = graph
                .nodes
                .get(&record.task)
                .ok_or(GraphError::Unknown(record.task))?;
            if node.runtime.current_attempt != Some(attempt)
                || node.runtime.lease_epoch != record.lease_epoch
            {
                return Err(GraphError::FencedAttempt(attempt));
            }
            // A completed attempt has to have produced what its task asked
            // for. Without this, "the child returned" and "the child answered
            // the question" are the same record, and the parent cannot tell
            // them apart.
            if outcome.state == AttemptState::Completed {
                if let Some(violation) = ResultContract::parse(&record.required_output)
                    .violation(outcome.result.as_ref())
                {
                    return Err(GraphError::ContractViolation(attempt, violation));
                }
            }
            Ok(json!({
                "attempt_id": attempt,
                "task_id": record.task,
                "state": outcome.state,
                "used": outcome.used,
                "reason": outcome.reason,
                "retryable": outcome.retryable,
                "result": outcome.result,
                "evidence": outcome.evidence,
                "ended_at_ms": outcome.ended_at_ms,
            }))
        })
    }

    /// Finish the task's running attempt, or the task itself when it has none.
    pub fn complete(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        self.close(id, TaskState::Completed, evidence)
    }

    /// Record why a task stopped, keeping whatever it produced.
    ///
    /// Idempotent, like `complete`: a caller that fails a task twice — a
    /// retry, a resumed process closing what it found — is describing the same
    /// history, not writing a second one.
    pub fn fail(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        self.close(id, TaskState::Failed, evidence)
    }

    /// Stop a task before it finished, with the reason.
    ///
    /// Allowed from any state, because cancelling is a decision about the
    /// future: a task that is pending never starts, and one that is running
    /// stops being anyone's to finish.
    pub fn cancel(&mut self, id: TaskId, reason: impl Into<String>) -> Result<(), GraphError> {
        self.close(id, TaskState::Cancelled, json!({"reason": reason.into()}))
    }

    /// Promote an execution-complete task to verified, citing the evidence.
    ///
    /// Separate from `complete` because they are different claims: the graph
    /// will not let "the child returned" stand in for "the check passed".
    ///
    /// The evidence has to be a completion proof for *this* task whose state
    /// is verified. That is the gate: a caller cannot mark work verified by
    /// passing a sentence about it, because the only shape this accepts is
    /// one the prover produces by reading recorded operations.
    pub fn verify(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        if self
            .nodes
            .get(&id)
            .is_some_and(|node| node.state == TaskState::Verified)
        {
            return Ok(());
        }
        let proved_task = evidence
            .get("task")
            .and_then(|task| serde_json::from_value::<TaskId>(task.clone()).ok());
        if proved_task != Some(id) {
            return Err(GraphError::NotAProof(
                id,
                "the evidence is not a completion proof for this task".into(),
            ));
        }
        match evidence.get("state").and_then(Value::as_str) {
            Some("verified") => {}
            Some(other) => {
                return Err(GraphError::NotAProof(id, format!("its proof says {other}")))
            }
            None => {
                return Err(GraphError::NotAProof(
                    id,
                    "the evidence carries no proof state".into(),
                ))
            }
        }
        self.transition(id, TaskState::Verified, Some(evidence))
    }

    /// Tasks waiting for someone to take them, in creation order.
    ///
    /// This is what makes a graph resumable: a process that died holding a
    /// lease leaves a task whose lease expires, and the next one finds it here
    /// with the goal it was created with.
    pub fn pending(&self) -> Vec<&TaskNode> {
        self.nodes
            .values()
            .filter(|node| matches!(node.state, TaskState::Ready | TaskState::Pending))
            .collect()
    }

    /// Already-ready tasks that can be resumed without promoting them again.
    pub fn runnable(&self) -> Vec<&TaskNode> {
        self.nodes
            .values()
            .filter(|node| {
                node.state == TaskState::Ready
                    && self.dependencies_completed(node)
                    && (node.runtime.plan.is_none() || self.is_plan_approved(node.id))
            })
            .collect()
    }

    /// Spend part of a task's allowance, against its running attempt.
    pub fn consume(&mut self, id: TaskId, used: Budget) -> Result<(), GraphError> {
        let available = self
            .nodes
            .get(&id)
            .ok_or(GraphError::Unknown(id))?
            .available();
        if !used.fits_within(available) {
            let evidence = PartialEvidence {
                reason: "budget_exhausted".into(),
                remaining: available,
            };
            self.commit("task.budget_exhausted", |_| {
                Ok(json!({"task_id": id, "partial_evidence": evidence}))
            })?;
            return Err(GraphError::BudgetExhausted(evidence));
        }
        self.commit("task.budget_used", |graph| {
            let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
            if !used.fits_within(node.available()) {
                return Err(GraphError::BudgetExhausted(PartialEvidence {
                    reason: "budget_exhausted".into(),
                    remaining: node.available(),
                }));
            }
            Ok(json!({"task_id": id, "used": used}))
        })
    }

    /// Hand every running task back to the queue, whatever its lease says.
    ///
    /// A lease expiring is how a *silent* holder is detected; this is for the
    /// case where the holder is known to be gone — its turn was found open by
    /// the process that came after it. Waiting out a lease we already know is
    /// dead would make resuming a killed run mean "come back in half an hour".
    pub fn reclaim_running(&mut self) -> Result<Vec<TaskId>, GraphError> {
        self.expire(
            self.nodes
                .iter()
                .filter_map(|(id, node)| (node.state == TaskState::Running).then_some(*id))
                .collect(),
            "reclaimed",
        )
    }

    pub fn recover_expired(&mut self, now_ms: u64) -> Result<Vec<TaskId>, GraphError> {
        self.expire(
            self.nodes
                .iter()
                .filter_map(|(id, node)| {
                    (node.state == TaskState::Running
                        && node
                            .lease_expires_at_ms
                            .is_some_and(|expiry| expiry <= now_ms))
                    .then_some(*id)
                })
                .collect(),
            "lease_expired",
        )
    }

    pub fn node(&self, id: TaskId) -> Option<&TaskNode> {
        self.nodes.get(&id)
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The stream this graph is a projection of, for a caller that needs to
    /// build a second projection over the same history.
    pub fn store(&self) -> Arc<dyn EventStore> {
        Arc::clone(&self.store)
    }

    /// Every task in the graph, in creation order.
    pub fn tasks(&self) -> impl Iterator<Item = &TaskNode> {
        self.nodes.values()
    }

    pub fn attempt(&self, id: AttemptId) -> Option<&TaskAttempt> {
        self.attempts.get(&id)
    }

    /// Every try of one task, oldest first.
    pub fn attempts_of(&self, id: TaskId) -> Vec<&TaskAttempt> {
        self.nodes
            .get(&id)
            .map(|node| {
                node.runtime
                    .attempts
                    .iter()
                    .filter_map(|attempt| self.attempts.get(attempt))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dependencies_completed(&self, node: &TaskNode) -> bool {
        node.dependencies.iter().all(|dependency| {
            self.nodes.get(dependency).is_some_and(|node| {
                matches!(node.state, TaskState::Completed | TaskState::Verified)
            })
        })
    }

    fn is_ready(&self, node: &TaskNode) -> bool {
        node.state == TaskState::Pending
            && (node.runtime.plan.is_none() || self.is_plan_approved(node.id))
            && self.dependencies_completed(node)
    }

    fn has_failed_dependency(&self, node: &TaskNode) -> bool {
        node.dependencies
            .iter()
            .any(|dependency| self.is_permanently_stopped(*dependency))
    }

    fn is_permanently_stopped(&self, id: TaskId) -> bool {
        self.nodes.get(&id).is_some_and(|node| match node.state {
            TaskState::Cancelled | TaskState::Blocked => true,
            TaskState::Failed => node
                .runtime
                .attempts
                .last()
                .and_then(|attempt| self.attempts.get(attempt))
                .is_none_or(|attempt| attempt.retryable != Retryability::Retryable),
            _ => false,
        })
    }

    /// Drain terminal roots to a fixed point, committing each blocked edge
    /// against the current stream version. Replay only queues roots; it never
    /// writes while folding events.
    fn reconcile_blocked(&mut self) -> Result<Vec<TaskId>, GraphError> {
        if self.reconciling {
            return Ok(Vec::new());
        }
        self.reconciling = true;
        let mut blocked = Vec::new();
        let result = (|| {
            let mut queue = std::mem::take(&mut self.block_seeds);
            let mut visited = BTreeSet::new();
            while let Some(id) = queue.pop_first() {
                if !visited.insert(id) {
                    continue;
                }
                if self.is_permanently_stopped(id) {
                    queue.extend(self.dependents.get(&id).into_iter().flatten().copied());
                } else {
                    let result = self.commit_if("task.transitioned", |graph| {
                        let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
                        if !matches!(node.state, TaskState::Pending | TaskState::Ready)
                            || !graph.has_failed_dependency(node)
                        {
                            return Ok(None);
                        }
                        Ok(Some(json!({
                            "task_id": id, "from": node.state, "to": TaskState::Blocked,
                            "evidence": null,
                        })))
                    });
                    match result {
                        Ok(true) => {
                            blocked.push(id);
                            queue.extend(self.dependents.get(&id).into_iter().flatten().copied());
                        }
                        Ok(false) => {}
                        Err(error) => {
                            queue.insert(id);
                            self.block_seeds.extend(queue);
                            return Err(error);
                        }
                    }
                }
                queue.append(&mut self.block_seeds);
            }
            Ok(blocked)
        })();
        self.reconciling = false;
        result
    }

    /// End a task, through its running attempt when it has one.
    fn close(&mut self, id: TaskId, target: TaskState, evidence: Value) -> Result<(), GraphError> {
        self.refresh()?;
        self.reconcile_blocked()?;
        let node = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
        if node.state == target {
            return Ok(());
        }
        let Some(attempt) = node.runtime.current_attempt else {
            return self.transition(id, target, Some(evidence));
        };
        let state = match target {
            TaskState::Completed => AttemptState::Completed,
            TaskState::Cancelled => AttemptState::Cancelled,
            _ => AttemptState::Failed,
        };
        self.finish_attempt(
            attempt,
            &AttemptOutcome {
                state,
                used: Budget::default(),
                reason: evidence
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                retryable: Retryability::NotRetryable,
                result: Some(evidence),
                evidence: Vec::new(),
                ended_at_ms: crate::artifact::unix_time_ms(),
            },
        )
    }

    /// Return a task to the queue and retire the attempt that held it.
    fn expire(&mut self, tasks: Vec<TaskId>, reason: &str) -> Result<Vec<TaskId>, GraphError> {
        for id in &tasks {
            let id = *id;
            self.commit("task.attempt_expired", |graph| {
                let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
                if node.state != TaskState::Running {
                    return Err(GraphError::InvalidTransition(node.state, TaskState::Ready));
                }
                Ok(json!({
                    "task_id": id,
                    "attempt_id": node.runtime.current_attempt,
                    "reason": reason,
                }))
            })?;
        }
        Ok(tasks)
    }

    fn transition(
        &mut self,
        id: TaskId,
        target: TaskState,
        evidence: Option<Value>,
    ) -> Result<(), GraphError> {
        self.commit_if("task.transitioned", |graph| {
            let node = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
            let current = node.state;
            if current == target {
                return Ok(None);
            }
            if !transition_allowed(current, target) {
                return Err(GraphError::InvalidTransition(current, target));
            }
            if target == TaskState::Ready && current == TaskState::Pending && !graph.is_ready(node)
            {
                return Err(GraphError::InvalidTransition(current, target));
            }
            Ok(Some(
                json!({"task_id": id, "from": current, "to": target, "evidence": evidence}),
            ))
        })?;
        Ok(())
    }

    fn would_cycle(&self, new: TaskId, dependencies: &[TaskId]) -> bool {
        let mut pending = dependencies.to_vec();
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == new {
                return true;
            }
            if seen.insert(id) {
                if let Some(node) = self.nodes.get(&id) {
                    pending.extend(&node.dependencies);
                }
            }
        }
        false
    }

    fn replay(&mut self, event: &EventEnvelope) -> Result<(), GraphError> {
        let EventPayload::Inline { data } = &event.payload else {
            return Ok(());
        };
        // Four tables rather than one, split by what each event is about.
        // An event nobody claims is not an error: a stream carries the turn
        // lifecycle and usage too, and a graph reads only its own part.
        let kind = event.kind.as_str();
        if kind == "todo.created" {
            return self.replay_todo_created(data);
        }
        match self.replay_lifecycle(kind, data) {
            Some(replayed) => replayed,
            None => match self.replay_attempt(kind, data) {
                Some(replayed) => replayed,
                None => match self.replay_acceptance(kind, data) {
                    Some(replayed) => replayed,
                    None => self.replay_workspace(kind, data).unwrap_or(Ok(())),
                },
            },
        }
    }

    /// Tasks themselves: created, moved, authorized, charged.
    fn replay_lifecycle(&mut self, kind: &str, data: &Value) -> Option<Result<(), GraphError>> {
        Some(match kind {
            "task.created" => self.replay_created(data),
            "task.plan_approved" => self.replay_plan_approved(data),
            "task.plan_intent" => self.replay_plan_intent(data),
            "task.scripted_plan_authorized" => self.replay_scripted_plan_authorized(data),
            "task.transitioned" => self.replay_transitioned(data),
            "task.authorized" | "task.authority_narrowed" => self.replay_authority(data),
            "task.budget_used" => self.replay_budget_used(data),
            _ => return None,
        })
    }

    /// One try of a task, and what it recorded along the way.
    fn replay_attempt(&mut self, kind: &str, data: &Value) -> Option<Result<(), GraphError>> {
        Some(match kind {
            "task.attempt_started" => self.replay_attempt_started(data),
            "task.attempt_finished" => self.replay_attempt_finished(data),
            "task.attempt_expired" => self.replay_attempt_expired(data),
            "task.attempt_state_changed" => self.replay_attempt_state(data),
            "task.attempt_cancel_requested" => self.replay_cancel_requested(data),
            "task.attempt_pause_requested" => self.replay_pause_requested(data),
            "task.attempt_resumed" => self.replay_resumed(data),
            "task.attempt_superseded" => self.replay_attempt_superseded(data),
            "task.attempt_trace" => self.replay_trace(data),
            _ => return None,
        })
    }

    /// What a task committed to, and what was said about it.
    fn replay_acceptance(&mut self, kind: &str, data: &Value) -> Option<Result<(), GraphError>> {
        Some(match kind {
            "task.criterion_declared" => self.replay_criterion(data),
            "task.criterion_judged" => self.replay_judgment(data),
            "task.criterion_retired" => self.replay_retired(data),
            "task.message_sent" => self.replay_message_sent(data),
            "task.messages_delivered" => self.replay_messages_delivered(data),
            _ => return None,
        })
    }

    /// Which view belongs to whom, and what a writer produced in it.
    fn replay_workspace(&mut self, kind: &str, data: &Value) -> Option<Result<(), GraphError>> {
        Some(match kind {
            "workspace.assigned" => self.replay_assigned(data),
            "workspace.released" => self.replay_released(data),
            "workspace.writer_result" => self.replay_writer_result(data),
            _ => return None,
        })
    }

    fn replay_todo_created(&mut self, data: &Value) -> Result<(), GraphError> {
        let Some(item) = data
            .get("item")
            .cloned()
            .and_then(|value| serde_json::from_value::<TodoItem>(value).ok())
        else {
            return Ok(());
        };
        self.todo_ids.insert(item.id.clone());
        if let (Some(root), Some(step_id)) = (
            data.get("plan_root")
                .cloned()
                .and_then(|value| serde_json::from_value::<TaskId>(value).ok()),
            data.get("step_id").and_then(Value::as_str),
        ) {
            self.plan_todos.insert((root, step_id.to_owned()), item.id);
        }
        Ok(())
    }

    fn replay_plan_intent(&mut self, data: &Value) -> Result<(), GraphError> {
        let root: TaskId = field(data, "root")?;
        let steps: Value = field(data, "steps")?;
        if !self.nodes.contains_key(&root) || !steps.is_array() {
            return Err(GraphError::InvalidEvent("invalid plan intent".into()));
        }
        self.plan_intents.insert(root, steps);
        Ok(())
    }
    fn replay_scripted_plan_authorized(&mut self, data: &Value) -> Result<(), GraphError> {
        let root: TaskId = field(data, "root")?;
        if !self.nodes.contains_key(&root) {
            return Err(GraphError::Unknown(root));
        }
        self.scripted_plan_roots.insert(root);
        Ok(())
    }

    fn replay_plan_approved(&mut self, data: &Value) -> Result<(), GraphError> {
        let ids: Vec<TaskId> = field(data, "task_ids")
            .map_err(|_| GraphError::InvalidEvent("plan approval has no task IDs".into()))?;
        if ids.is_empty() {
            return Err(GraphError::InvalidEvent(
                "plan approval has no task IDs".into(),
            ));
        }
        let note = match data.get("note") {
            None | Some(Value::Null) => None,
            Some(Value::String(note)) => Some(note.clone()),
            _ => {
                return Err(GraphError::InvalidEvent(
                    "plan approval has invalid note".into(),
                ))
            }
        };
        for &id in &ids {
            self.validate_plan_task(id)?;
            if self
                .approved_plan
                .get(&id)
                .is_some_and(|recorded| recorded.as_deref() != note.as_deref())
            {
                return Err(GraphError::PlanApprovalConflict(id));
            }
        }
        self.approved_plan
            .extend(ids.into_iter().map(|id| (id, note.clone())));
        Ok(())
    }

    fn replay_created(&mut self, data: &Value) -> Result<(), GraphError> {
        let node: TaskNode = field(data, "node")
            .map_err(|_| GraphError::InvalidEvent("task.created has no node".into()))?;
        if let Some(parent) = node.runtime.parent {
            // The parent holds the child's whole allowance until the child
            // settles, so a sibling cannot be promised it too.
            let budget = node.budget;
            if let Some(parent) = self.nodes.get_mut(&parent) {
                parent.runtime.reserved = parent.runtime.reserved.saturating_add(budget);
            }
        }
        let blocked = matches!(node.state, TaskState::Cancelled | TaskState::Blocked)
            || (node.state == TaskState::Failed && node.runtime.attempts.is_empty());
        if blocked {
            self.block_seeds.insert(node.id);
        }
        for dependency in &node.dependencies {
            self.dependents
                .entry(*dependency)
                .or_default()
                .insert(node.id);
            if self.is_permanently_stopped(*dependency) {
                self.block_seeds.insert(*dependency);
            }
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }

    fn replay_transitioned(&mut self, data: &Value) -> Result<(), GraphError> {
        let id = event_task_id(data)?;
        let state: TaskState = field(data, "to")
            .map_err(|_| GraphError::InvalidEvent("transition has no target".into()))?;
        self.nodes
            .get_mut(&id)
            .ok_or(GraphError::Unknown(id))?
            .state = state;
        if self.is_permanently_stopped(id) {
            self.block_seeds.insert(id);
        }
        if matches!(
            state,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        ) {
            self.settle(id);
        }
        Ok(())
    }

    fn replay_attempt_started(&mut self, data: &Value) -> Result<(), GraphError> {
        let attempt: TaskAttempt = field(data, "attempt")
            .map_err(|_| GraphError::InvalidEvent("attempt event has no attempt".into()))?;
        let node = self
            .nodes
            .get_mut(&attempt.task)
            .ok_or(GraphError::Unknown(attempt.task))?;
        node.assignee = Some(attempt.assignee);
        node.lease_expires_at_ms = Some(attempt.lease_expires_at_ms);
        node.state = TaskState::Running;
        node.runtime.lease_epoch = attempt.lease_epoch;
        node.runtime.current_attempt = Some(attempt.id);
        node.runtime.attempts.push(attempt.id);
        self.attempts.insert(attempt.id, attempt);
        Ok(())
    }

    fn replay_attempt_finished(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("finish event has no attempt".into()))?;
        let state: AttemptState = field(data, "state")
            .map_err(|_| GraphError::InvalidEvent("finish event has no state".into()))?;
        let used: Budget = field(data, "used").unwrap_or_default();
        let attempt = self
            .attempts
            .get_mut(&id)
            .ok_or(GraphError::UnknownAttempt(id))?;
        attempt.state = state;
        attempt.used = attempt.used.saturating_add(used);
        attempt.ended_at_ms = data.get("ended_at_ms").and_then(Value::as_u64);
        attempt.terminal_reason = data
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        attempt.retryable = field(data, "retryable").unwrap_or_default();
        attempt.result = data.get("result").cloned().filter(|value| !value.is_null());
        attempt.evidence = field(data, "evidence").unwrap_or_default();
        let task = attempt.task;
        let terminal_failure = state == AttemptState::Cancelled
            || (state == AttemptState::Failed && attempt.retryable != Retryability::Retryable);
        let node = self.nodes.get_mut(&task).ok_or(GraphError::Unknown(task))?;
        node.runtime.used = node.runtime.used.saturating_add(used);
        node.runtime.current_attempt = None;
        node.lease_expires_at_ms = None;
        node.state = match state {
            AttemptState::Completed => TaskState::Completed,
            AttemptState::Cancelled => TaskState::Cancelled,
            _ => TaskState::Failed,
        };
        self.settle(task);
        if terminal_failure {
            self.block_seeds.insert(task);
        }
        Ok(())
    }

    fn replay_attempt_expired(&mut self, data: &Value) -> Result<(), GraphError> {
        let id = event_task_id(data)?;
        if let Some(attempt) = data
            .get("attempt_id")
            .and_then(|value| serde_json::from_value::<AttemptId>(value.clone()).ok())
            .and_then(|attempt| self.attempts.get_mut(&attempt))
        {
            attempt.state = AttemptState::Expired;
            attempt.terminal_reason = data
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        let node = self.nodes.get_mut(&id).ok_or(GraphError::Unknown(id))?;
        node.state = TaskState::Ready;
        node.assignee = None;
        node.lease_expires_at_ms = None;
        node.runtime.current_attempt = None;
        Ok(())
    }

    fn replay_attempt_state(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("state event has no attempt".into()))?;
        let state: AttemptState = field(data, "state")
            .map_err(|_| GraphError::InvalidEvent("state event has no state".into()))?;
        self.attempts
            .get_mut(&id)
            .ok_or(GraphError::UnknownAttempt(id))?
            .state = state;
        Ok(())
    }

    fn replay_cancel_requested(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("cancel event has no attempt".into()))?;
        let attempt = self
            .attempts
            .get_mut(&id)
            .ok_or(GraphError::UnknownAttempt(id))?;
        attempt.state = AttemptState::Cancelling;
        attempt.terminal_reason = data
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(())
    }

    fn replay_pause_requested(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("pause event has no attempt".into()))?;
        if !self.attempts.contains_key(&id) {
            return Err(GraphError::UnknownAttempt(id));
        }
        self.paused.insert(id);
        Ok(())
    }

    fn replay_resumed(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("resume event has no attempt".into()))?;
        self.paused.remove(&id);
        Ok(())
    }

    fn replay_attempt_superseded(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("supersede event has no attempt".into()))?;
        let attempt = self
            .attempts
            .get_mut(&id)
            .ok_or(GraphError::UnknownAttempt(id))?;
        attempt.state = AttemptState::Superseded;
        let task = attempt.task;
        if let Some(node) = self.nodes.get_mut(&task) {
            node.runtime.current_attempt = None;
            node.lease_expires_at_ms = None;
        }
        Ok(())
    }

    fn replay_criterion(&mut self, data: &Value) -> Result<(), GraphError> {
        let criterion: AcceptanceCriterion = field(data, "criterion")
            .map_err(|_| GraphError::InvalidEvent("criterion event has no criterion".into()))?;
        self.criteria.insert(criterion.id, criterion);
        Ok(())
    }

    fn replay_judgment(&mut self, data: &Value) -> Result<(), GraphError> {
        let judgment: Judgment = field(data, "judgment")
            .map_err(|_| GraphError::InvalidEvent("judgment event has no judgment".into()))?;
        self.judgments
            .entry(judgment.criterion)
            .or_default()
            .push(judgment);
        Ok(())
    }

    fn replay_retired(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: CriterionId = field(data, "criterion_id")
            .map_err(|_| GraphError::InvalidEvent("retire event has no criterion".into()))?;
        self.criteria
            .get_mut(&id)
            .ok_or(GraphError::UnknownCriterion(id))?
            .inapplicable = data.get("why").and_then(Value::as_str).map(str::to_owned);
        Ok(())
    }

    fn replay_assigned(&mut self, data: &Value) -> Result<(), GraphError> {
        let assignment: WorkspaceAssignment = field(data, "assignment")
            .map_err(|_| GraphError::InvalidEvent("assignment event has no assignment".into()))?;
        self.assignments.insert(assignment.id, assignment);
        Ok(())
    }

    fn replay_released(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AssignmentId = field(data, "assignment_id")
            .map_err(|_| GraphError::InvalidEvent("release event has no assignment".into()))?;
        self.assignments
            .get_mut(&id)
            .ok_or(GraphError::UnknownAssignment(id))?
            .released = true;
        Ok(())
    }

    fn replay_writer_result(&mut self, data: &Value) -> Result<(), GraphError> {
        let result: WriterResult = field(data, "result")
            .map_err(|_| GraphError::InvalidEvent("writer event has no result".into()))?;
        self.writer_results.insert(result.assignment, result);
        Ok(())
    }

    fn replay_trace(&mut self, data: &Value) -> Result<(), GraphError> {
        let id: AttemptId = field(data, "attempt_id")
            .map_err(|_| GraphError::InvalidEvent("trace event has no attempt".into()))?;
        let entries = self.traces.entry(id).or_default();
        entries.push(json!({
            "kind": data.get("kind").and_then(Value::as_str).unwrap_or_default(),
            "data": data.get("data").cloned().unwrap_or(Value::Null),
        }));
        if entries.len() > MAX_TRACE_ENTRIES {
            entries.remove(0);
        }
        Ok(())
    }

    fn replay_message_sent(&mut self, data: &Value) -> Result<(), GraphError> {
        let message: TaskMessage = field(data, "message")
            .map_err(|_| GraphError::InvalidEvent("message event has no message".into()))?;
        self.inbox.entry(message.to).or_default().push(message.id);
        self.messages.insert(message.id, message);
        Ok(())
    }

    fn replay_messages_delivered(&mut self, data: &Value) -> Result<(), GraphError> {
        let to = event_task_id(data)?;
        let ids: Vec<MessageId> = field(data, "message_ids")
            .map_err(|_| GraphError::InvalidEvent("delivery event has no messages".into()))?;
        let at = data.get("delivered_at_ms").and_then(Value::as_u64);
        for id in &ids {
            if let Some(message) = self.messages.get_mut(id) {
                message.delivered_at_ms = at;
            }
        }
        if let Some(waiting) = self.inbox.get_mut(&to) {
            waiting.retain(|id| !ids.contains(id));
        }
        Ok(())
    }

    fn replay_authority(&mut self, data: &Value) -> Result<(), GraphError> {
        let id = event_task_id(data)?;
        let authority: Vec<CapabilityGrant> = field(data, "authority")
            .map_err(|_| GraphError::InvalidEvent("authority event has no authority".into()))?;
        self.nodes
            .get_mut(&id)
            .ok_or(GraphError::Unknown(id))?
            .authority = authority;
        Ok(())
    }

    fn replay_budget_used(&mut self, data: &Value) -> Result<(), GraphError> {
        let id = event_task_id(data)?;
        let used: Budget = field(data, "used")
            .map_err(|_| GraphError::InvalidEvent("budget event has no usage".into()))?;
        let node = self.nodes.get_mut(&id).ok_or(GraphError::Unknown(id))?;
        node.runtime.used = node.runtime.used.saturating_add(used);
        if let Some(attempt) = node
            .runtime
            .current_attempt
            .and_then(|attempt| self.attempts.get_mut(&attempt))
        {
            attempt.used = attempt.used.saturating_add(used);
        }
        Ok(())
    }

    /// Give the parent back what a settled child did not spend.
    ///
    /// Once per child, because a reservation released twice would hand the
    /// parent capacity it never had.
    fn settle(&mut self, id: TaskId) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if node.runtime.settled {
            return;
        }
        node.runtime.settled = true;
        let (Some(parent), reserved, used) = (node.runtime.parent, node.budget, node.runtime.used)
        else {
            return;
        };
        if let Some(parent) = self.nodes.get_mut(&parent) {
            parent.runtime.reserved = parent.runtime.reserved.saturating_sub(reserved);
            parent.runtime.used = parent.runtime.used.saturating_add(used);
        }
    }

    /// Append one graph event, revalidating it if the stream moved.
    ///
    /// A task graph shares its session's stream with whatever else writes to
    /// it — the turn lifecycle, usage — because resuming a task means replaying
    /// one history, not correlating two. So a conflict here is the normal case
    /// of "something else appended since we last looked", not a lost update.
    ///
    /// What was missed may still change the answer: another writer may have
    /// spent the budget this event reserves, or retried the attempt it closes.
    /// So the command is a closure over the current projection, rebuilt and
    /// rechecked after every catch-up rather than replayed blindly.
    fn commit(
        &mut self,
        kind: &str,
        build: impl Fn(&Self) -> Result<Value, GraphError>,
    ) -> Result<(), GraphError> {
        self.commit_if(kind, |graph| build(graph).map(Some))?;
        Ok(())
    }

    /// Like `commit`, but permit an already-settled transition to be skipped
    /// after catching up with a competing writer.
    fn commit_if(
        &mut self,
        kind: &str,
        build: impl Fn(&Self) -> Result<Option<Value>, GraphError>,
    ) -> Result<bool, GraphError> {
        self.reconcile_blocked()?;
        for attempt in 0..COMMIT_ATTEMPTS {
            // Catch up before building, not only after a conflict. A second
            // view of the same session — a worker's, a scheduler's — has not
            // seen what the first one appended, and a command built against
            // that gap refuses work that does exist rather than conflicting.
            if self.store.current_version(self.session)?.0 > self.version.0 {
                self.catch_up()?;
            }
            self.reconcile_blocked()?;
            let Some(payload) = build(self)? else {
                return Ok(false);
            };
            let sequence = self.version.0.checked_add(1).ok_or(GraphError::Overflow)?;
            let event = EventEnvelope::new(
                self.session,
                sequence,
                self.actor.clone(),
                self.last_event,
                CorrelationId::new(),
                SchemaVersion(1),
                kind,
                EventPayload::Inline { data: payload },
            );
            match self
                .store
                .append(self.session, self.version, vec![event.clone()])
            {
                Ok(version) => {
                    self.version = version;
                    self.replay(&event)?;
                    self.last_event = Some(event.id);
                    self.reconcile_blocked()?;
                    return Ok(true);
                }
                Err(StoreError::Conflict { .. }) if attempt + 1 < COMMIT_ATTEMPTS => {
                    self.catch_up()?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(GraphError::Store(StoreError::Conflict {
            expected: self.version,
            actual: self.store.current_version(self.session)?,
        }))
    }

    /// Replay everything appended to this session since the graph last looked.
    fn catch_up(&mut self) -> Result<(), GraphError> {
        loop {
            let page = self.store.read(
                self.session,
                self.version.0.checked_add(1).ok_or(GraphError::Overflow)?,
                MAX_CATCH_UP_BATCH,
            )?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            let version = StreamVersion(last.sequence);
            for event in &page {
                self.replay(event)?;
                self.last_event = Some(event.id);
            }
            self.version = version;
        }
    }
}

const fn transition_allowed(current: TaskState, target: TaskState) -> bool {
    matches!(
        (current, target),
        (TaskState::Pending, TaskState::Ready)
            | (TaskState::Ready, TaskState::Running)
            | (TaskState::Pending | TaskState::Ready, TaskState::Failed)
            | (TaskState::Running, TaskState::Completed | TaskState::Failed)
            | (TaskState::Completed, TaskState::Verified)
            // A retry brings a failed task back to the queue. Only a failed
            // one: cancelling is a decision about the future, so a retry must
            // not be a way to undo it.
            | (TaskState::Failed, TaskState::Ready)
            | (TaskState::Pending | TaskState::Ready, TaskState::Blocked)
            | (TaskState::Pending | TaskState::Ready | TaskState::Running, TaskState::Cancelled)
    )
}

/// Undelivered messages one task may hold. A sender past this is outrunning
/// the recipient, which is a thing to report rather than to buffer.
const MAX_INBOX: usize = 64;

/// Trace entries kept in memory per attempt. The stream keeps every one; this
/// is only how much of the tail a live reader gets without paging.
const MAX_TRACE_ENTRIES: usize = 256;

/// Events replayed per catch-up read. The same bound the service uses to page
/// a stream, for the same reason: a long session must not be read at once.
const MAX_CATCH_UP_BATCH: usize = 256;

/// Tries at appending one command. Two catch-ups and a third rebuild: past
/// that the stream is genuinely contended rather than merely busy.
const COMMIT_ATTEMPTS: usize = 3;

fn field<T: serde::de::DeserializeOwned>(data: &Value, name: &str) -> Result<T, GraphError> {
    serde_json::from_value(
        data.get(name)
            .cloned()
            .ok_or_else(|| GraphError::InvalidEvent(format!("event has no {name}")))?,
    )
    .map_err(|error| GraphError::InvalidEvent(error.to_string()))
}

pub fn attenuate_child_grant(
    parent: &CapabilityGrant,
    child: AgentId,
    action: CapabilityAction,
    scope: &ResourceScope,
    expiry: Option<u64>,
) -> Result<CapabilityGrant, AttenuationError> {
    parent.attenuate(Principal::Agent(child), action, scope, expiry)
}

#[derive(Debug)]
pub enum GraphError {
    /// A task's authority is written once, while nothing has derived from it.
    AlreadyAuthorized(TaskId),
    Duplicate(TaskId),
    Unknown(TaskId),
    UnknownAttempt(AttemptId),
    NotAPlanTask(TaskId),
    PlanNotApproved(TaskId),
    PlanApprovalConflict(TaskId),
    /// A result from an attempt that is no longer the task's current one.
    FencedAttempt(AttemptId),
    NotTerminal(AttemptState),
    /// A state an attempt cannot be moved to while it is still live.
    NotLive(AttemptState),
    /// The holder was already asked to stop.
    Cancelling(AttemptId),
    /// A completed attempt produced something its task did not ask for.
    ContractViolation(AttemptId, String),
    NotRetryable(AttemptId, Retryability),
    RetriesExhausted(TaskId),
    NothingToRetry(TaskId),
    StillRunning(AttemptId),
    /// A message for a task that has already ended.
    Ended(TaskId, TaskState),
    InboxFull(TaskId),
    UnknownAssignment(AssignmentId),
    /// Two writers cannot hold one view.
    ViewAlreadyOwned(AgentId, String),
    /// A writer's view is never the workspace an operator is looking at.
    WriterOnSource(String),
    /// The source had uncommitted work and the policy in force refuses it.
    DirtySource(String),
    NotAWriter(AssignmentId),
    DuplicateCriterion(CriterionId),
    UnknownCriterion(CriterionId),
    /// A verdict on a criterion a check decides, not a person.
    NotAJudgment(CriterionId),
    /// Verification was offered something that is not a proof of this task.
    NotAProof(TaskId, String),
    Cycle(TaskId),
    InvalidTransition(TaskState, TaskState),
    BudgetExhausted(PartialEvidence),
    BudgetExpansion,
    MissingAssignee,
    MissingParentGrant,
    Attenuation(AttenuationError),
    InvalidEvent(String),
    Overflow,
    Store(StoreError),
}

impl GraphError {
    /// The message for a variant that has nothing to interpolate.
    ///
    /// Also where [`Display`](fmt::Display) lands a variant none of its
    /// tables claimed, so a new one without a message reads as "task graph
    /// error" rather than as nothing at all.
    const fn constant(&self) -> &'static str {
        match self {
            Self::BudgetExhausted(_) => "task budget exhausted with partial evidence",
            Self::BudgetExpansion => "child budget exceeds the parent's remaining budget",
            Self::MissingAssignee => "delegated child task requires an assignee",
            Self::MissingParentGrant => "child requested an unknown parent grant",
            Self::Overflow => "task event sequence overflow",
            _ => "task graph error",
        }
    }
}

impl GraphError {
    /// Something about a task itself.
    fn describe_task(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        Some(match self {
            Self::AlreadyAuthorized(id) => {
                write!(formatter, "task {id} already holds recorded authority")
            }
            Self::Duplicate(id) => write!(formatter, "task {id} already exists"),
            Self::NotAPlanTask(id) => write!(formatter, "task {id} is not a runnable plan step"),
            Self::PlanNotApproved(id) => {
                write!(formatter, "plan task {id} has no operator approval")
            }
            Self::PlanApprovalConflict(id) => {
                write!(
                    formatter,
                    "plan task {id} is approved with a different operator note"
                )
            }
            Self::Unknown(id) => write!(formatter, "task {id} does not exist"),
            Self::Cycle(id) => write!(formatter, "task {id} introduces a dependency cycle"),
            Self::InvalidTransition(from, to) => {
                write!(formatter, "invalid task transition {from:?} -> {to:?}")
            }
            Self::Ended(id, state) => {
                write!(formatter, "task {id} is {state:?} and takes no messages")
            }
            Self::InboxFull(id) => write!(formatter, "task {id} has too many undelivered messages"),
            _ => return None,
        })
    }

    /// Something about one try of a task.
    fn describe_attempt(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        Some(match self {
            Self::UnknownAttempt(id) => write!(formatter, "attempt {id} does not exist"),
            Self::FencedAttempt(id) => write!(
                formatter,
                "attempt {id} has been superseded and cannot commit a result"
            ),
            Self::NotTerminal(state) => write!(formatter, "{state:?} does not end an attempt"),
            Self::NotLive(state) => write!(formatter, "{state:?} is not a live attempt state"),
            Self::Cancelling(id) => {
                write!(formatter, "attempt {id} has already been asked to stop")
            }
            Self::ContractViolation(id, why) => write!(
                formatter,
                "attempt {id} did not meet its required output: {why}"
            ),
            Self::NotRetryable(id, how) => {
                write!(formatter, "attempt {id} is {how:?} and will not be retried")
            }
            Self::RetriesExhausted(id) => write!(formatter, "task {id} has no retries left"),
            Self::NothingToRetry(id) => write!(formatter, "task {id} has no attempt to retry"),
            Self::StillRunning(id) => write!(formatter, "attempt {id} has not ended"),
            _ => return None,
        })
    }

    /// Something about a view, a criterion, or a proof.
    fn describe_claim(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        Some(match self {
            Self::UnknownAssignment(id) => {
                write!(formatter, "workspace assignment {id} does not exist")
            }
            Self::ViewAlreadyOwned(owner, view) => {
                write!(formatter, "agent {owner} already writes {view}")
            }
            Self::WriterOnSource(path) => write!(
                formatter,
                "a writer cannot be assigned the canonical workspace {path}"
            ),
            Self::DirtySource(path) => write!(
                formatter,
                "{path} has uncommitted work and this policy refuses to start from it"
            ),
            Self::NotAWriter(id) => write!(
                formatter,
                "assignment {id} is read-only and produces no result"
            ),
            Self::DuplicateCriterion(id) => write!(formatter, "criterion {id} already exists"),
            Self::UnknownCriterion(id) => write!(formatter, "criterion {id} does not exist"),
            Self::NotAJudgment(id) => write!(
                formatter,
                "criterion {id} is decided by a check, not by a verdict"
            ),
            Self::NotAProof(id, why) => write!(formatter, "task {id} cannot be verified: {why}"),
            _ => return None,
        })
    }
}

impl fmt::Display for GraphError {
    /// Three tables by subject, then what is left: an error someone else
    /// wrote, and the ones whose whole message is a constant.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(written) = self
            .describe_task(formatter)
            .or_else(|| self.describe_attempt(formatter))
            .or_else(|| self.describe_claim(formatter))
        {
            return written;
        }
        match self {
            Self::InvalidEvent(message) => write!(formatter, "invalid task event: {message}"),
            Self::Attenuation(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            fixed => formatter.write_str(fixed.constant()),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<StoreError> for GraphError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

fn event_task_id(data: &Value) -> Result<TaskId, GraphError> {
    field(data, "task_id").map_err(|_| GraphError::InvalidEvent("event has no task ID".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MemoryEventStore;

    fn node(id: TaskId, dependencies: Vec<TaskId>, budget: Budget) -> TaskNode {
        TaskNode {
            id,
            goal: "bounded task".into(),
            dependencies,
            assignee: None,
            required_output: "evidence".into(),
            workspace: WorkspaceRequirement::ReadOnlySnapshot,
            budget,
            authority: Vec::new(),
            state: TaskState::Pending,
            lease_expires_at_ms: None,
            runtime: TaskRuntime::default(),
        }
    }

    fn budget(each: u64) -> Budget {
        Budget {
            tokens: each,
            cost_micros: each,
            wall_ms: each,
        }
    }

    fn open(store: &Arc<dyn EventStore>, session: SessionId) -> TaskGraph {
        TaskGraph::new(Arc::clone(store), session, Principal::System).unwrap()
    }

    fn assignment(
        task: TaskId,
        attempt: AttemptId,
        view: &str,
        mutable: bool,
    ) -> WorkspaceAssignment {
        WorkspaceAssignment {
            id: AssignmentId::new(),
            task,
            attempt,
            owner: AgentId::new(),
            repository: "root-commit".into(),
            source: "/repo".into(),
            base_revision: "a".repeat(40),
            view: view.into(),
            backend: IsolationBackend::GitWorktree,
            mutable,
            lease_epoch: 1,
            expires_at_ms: 1_000,
            dirty: DirtyDisposition::Clean,
            released: false,
        }
    }

    #[test]
    fn one_mutable_view_has_exactly_one_writer() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let (first, second) = (TaskId::new(), TaskId::new());
        graph.add(node(first, Vec::new(), budget(10))).unwrap();
        graph.add(node(second, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let one = graph.lease(first, AgentId::new(), 1_000).unwrap();
        let two = graph.lease(second, AgentId::new(), 1_000).unwrap();

        let held = graph
            .assign_workspace(assignment(first, one, "/views/a", true))
            .unwrap();
        assert!(matches!(
            graph.assign_workspace(assignment(second, two, "/views/a", true)),
            Err(GraphError::ViewAlreadyOwned(_, _))
        ));
        // A reader cannot share a view with a writer either: the writer is
        // changing it underneath them.
        assert!(matches!(
            graph.assign_workspace(assignment(second, two, "/views/a", false)),
            Err(GraphError::ViewAlreadyOwned(_, _))
        ));
        // Released, so the next writer may have it.
        graph.release_workspace(held, "finished").unwrap();
        graph
            .assign_workspace(assignment(second, two, "/views/a", true))
            .unwrap();

        let replayed = open(&store, session);
        assert_eq!(replayed.live_assignments().count(), 1);
    }

    #[test]
    fn writer_authority_never_covers_the_canonical_workspace() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 1_000).unwrap();
        assert!(matches!(
            graph.assign_workspace(WorkspaceAssignment {
                view: "/repo".into(),
                ..assignment(id, attempt, "/views/a", true)
            }),
            Err(GraphError::WriterOnSource(_))
        ));
        // A reader may share the canonical path, because it changes nothing.
        graph
            .assign_workspace(WorkspaceAssignment {
                view: "/repo".into(),
                ..assignment(id, attempt, "/views/a", false)
            })
            .unwrap();
    }

    #[test]
    fn a_refused_dirty_source_produces_no_claim_at_all() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 1_000).unwrap();
        assert!(matches!(
            graph.assign_workspace(WorkspaceAssignment {
                dirty: DirtyDisposition::Refused,
                ..assignment(id, attempt, "/views/a", true)
            }),
            Err(GraphError::DirtySource(_))
        ));
        assert_eq!(graph.live_assignments().count(), 0);
    }

    #[test]
    fn a_view_whose_attempt_ended_is_reported_as_abandoned() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 1_000).unwrap();
        let held = graph
            .assign_workspace(assignment(id, attempt, "/views/a", true))
            .unwrap();
        graph
            .record_writer_result(&WriterResult {
                assignment: held,
                base_revision: "a".repeat(40),
                head_revision: Some("b".repeat(40)),
                patch_artifact: None,
                changed_files: vec!["src/lib.rs".into()],
                validation: vec!["artifact-check".into()],
                unresolved: Vec::new(),
            })
            .unwrap();
        assert!(graph.abandoned_assignments(0).is_empty());
        graph.complete(id, json!("done")).unwrap();

        // Restarted: the view is still there, still owned, and nobody is
        // running it. That is the set a recovery pass has to decide about.
        let replayed = open(&store, session);
        let abandoned = replayed.abandoned_assignments(0);
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].id, held);
        assert_eq!(
            replayed
                .writer_result(held)
                .map(|result| result.changed_files.clone()),
            Some(vec!["src/lib.rs".to_owned()])
        );
    }

    #[test]
    fn an_attempts_trace_survives_a_restart_and_keeps_its_tail() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let attempt = {
            let mut graph = open(&store, session);
            let id = TaskId::new();
            graph.add(node(id, Vec::new(), budget(10))).unwrap();
            graph.ready().unwrap();
            let attempt = graph.lease(id, AgentId::new(), 1_000).unwrap();
            // More than the projection keeps, so the trimming is exercised
            // rather than assumed.
            for index in 0..MAX_TRACE_ENTRIES + 3 {
                graph
                    .record(attempt, "model.tool_call", json!({"call": index}))
                    .unwrap();
            }
            attempt
        };

        let replayed = open(&store, session);
        let trace = replayed.trace_of(attempt);
        assert_eq!(trace.len(), MAX_TRACE_ENTRIES);
        // The tail, not the head: what a child did last is what explains
        // where it stopped.
        assert_eq!(trace[0]["data"]["call"], json!(3));
        assert_eq!(
            trace[MAX_TRACE_ENTRIES - 1]["data"]["call"],
            json!(MAX_TRACE_ENTRIES + 2)
        );
        assert_eq!(trace[0]["kind"], json!("model.tool_call"));
        // An attempt nobody recorded against has nothing, rather than a gap
        // that has to be told from an empty one.
        assert!(replayed.trace_of(AttemptId::new()).is_empty());
    }

    #[test]
    fn messages_are_ordered_delivered_once_and_survive_a_restart() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let parent = TaskId::new();
        let child = TaskId::new();
        graph.add(node(parent, Vec::new(), budget(100))).unwrap();
        graph.add(node(child, Vec::new(), budget(10))).unwrap();

        let first = graph
            .send(
                parent,
                child,
                MessageKind::Instruction,
                json!({"note": "narrow it to the parser"}),
                None,
                1,
            )
            .unwrap();
        let second = graph
            .send(
                parent,
                child,
                MessageKind::Progress,
                json!("still here"),
                Some(first),
                2,
            )
            .unwrap();
        assert_eq!(
            graph.inbox(child).iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![first, second]
        );

        let delivered = graph.deliver(child, 3).unwrap();
        assert_eq!(delivered.len(), 2);
        // Taken once: a second read is not a redelivery.
        assert!(graph.inbox(child).is_empty());
        assert!(graph.deliver(child, 4).unwrap().is_empty());

        // A message carries no authority, and the record says so by having
        // nowhere to put one: what survives a restart is what was said.
        let replayed = open(&store, session);
        assert_eq!(
            replayed.message(first).unwrap().kind,
            MessageKind::Instruction
        );
        assert_eq!(replayed.message(second).unwrap().causation, Some(first));
        assert_eq!(replayed.message(second).unwrap().delivered_at_ms, Some(3));
        assert!(replayed.inbox(child).is_empty());
    }

    #[test]
    fn a_task_that_ended_takes_no_more_messages() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let parent = TaskId::new();
        let child = TaskId::new();
        graph.add(node(parent, Vec::new(), budget(100))).unwrap();
        graph.add(node(child, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        graph.lease(child, AgentId::new(), 10).unwrap();
        graph.complete(child, json!("answered")).unwrap();
        assert!(matches!(
            graph.send(
                parent,
                child,
                MessageKind::Instruction,
                json!("more"),
                None,
                1
            ),
            Err(GraphError::Ended(_, TaskState::Completed))
        ));
    }

    #[test]
    fn a_full_inbox_refuses_rather_than_growing() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let parent = TaskId::new();
        let child = TaskId::new();
        graph.add(node(parent, Vec::new(), budget(100))).unwrap();
        graph.add(node(child, Vec::new(), budget(10))).unwrap();
        for index in 0..MAX_INBOX {
            graph
                .send(parent, child, MessageKind::Progress, json!(index), None, 1)
                .unwrap();
        }
        assert!(matches!(
            graph.send(
                parent,
                child,
                MessageKind::Progress,
                json!("one too many"),
                None,
                1
            ),
            Err(GraphError::InboxFull(_))
        ));
    }

    #[test]
    fn a_completed_attempt_has_to_produce_what_its_task_asked_for() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let id = TaskId::new();
        graph
            .add(TaskNode {
                required_output: json!({
                    "type": "object",
                    "required": ["finding", "evidence"],
                })
                .to_string(),
                ..node(id, Vec::new(), budget(10))
            })
            .unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 10).unwrap();
        let violation = graph.finish_attempt(
            attempt,
            &AttemptOutcome::completed(Budget::default(), json!({"finding": "here"}), 1),
        );
        assert!(matches!(
            violation,
            Err(GraphError::ContractViolation(_, _))
        ));
        // Refused, so the attempt is still the task's to finish properly.
        graph
            .finish_attempt(
                attempt,
                &AttemptOutcome::completed(
                    Budget::default(),
                    json!({"finding": "here", "evidence": ["artifact-1"]}),
                    2,
                ),
            )
            .unwrap();
        assert_eq!(graph.node(id).unwrap().state, TaskState::Completed);
    }

    #[test]
    fn a_cancelled_attempt_cannot_go_back_to_running() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = open(&store, SessionId::new());
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 10).unwrap();
        graph
            .advance_attempt(attempt, AttemptState::Running)
            .unwrap();
        graph.request_cancel(attempt, "operator").unwrap();
        assert!(matches!(
            graph.advance_attempt(attempt, AttemptState::Running),
            Err(GraphError::Cancelling(_))
        ));
    }

    #[test]
    fn a_restart_finds_the_cancellation_that_was_requested() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let attempt = {
            let mut graph = open(&store, session);
            let id = TaskId::new();
            graph.add(node(id, Vec::new(), budget(10))).unwrap();
            graph.ready().unwrap();
            let attempt = graph.lease(id, AgentId::new(), 10).unwrap();
            graph
                .request_cancel(attempt, "operator changed their mind")
                .unwrap();
            attempt
        };
        let replayed = open(&store, session);
        let record = replayed.attempt(attempt).unwrap();
        assert_eq!(record.state, AttemptState::Cancelling);
        assert_eq!(
            record.terminal_reason.as_deref(),
            Some("operator changed their mind")
        );
    }

    #[test]
    fn pause_and_resume_are_replayable_and_fenced_to_a_live_attempt() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(id, AgentId::new(), 10).unwrap();

        graph.request_pause(attempt).unwrap();
        assert!(graph.is_paused(attempt));
        assert!(open(&store, session).is_paused(attempt));

        graph.resume(attempt).unwrap();
        assert!(!open(&store, session).is_paused(attempt));
        graph.request_cancel(attempt, "done").unwrap();
        assert!(matches!(
            graph.request_pause(attempt),
            Err(GraphError::FencedAttempt(_))
        ));
    }

    #[test]
    fn transitions_cycles_leases_and_budgets_are_durable_and_bounded() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let first = TaskId::new();
        graph.add(node(first, Vec::new(), budget(10))).unwrap();
        assert_eq!(graph.ready().unwrap(), vec![first]);
        graph.lease(first, AgentId::new(), 5).unwrap();
        assert_eq!(graph.recover_expired(5).unwrap(), vec![first]);
        graph.lease(first, AgentId::new(), 10).unwrap();
        assert!(matches!(
            graph.consume(first, budget(11)),
            Err(GraphError::BudgetExhausted(_))
        ));
        graph.consume(first, budget(4)).unwrap();
        graph
            .complete(first, json!({"artifact": "partial"}))
            .unwrap();
        graph
            .complete(first, json!({"artifact": "duplicate"}))
            .unwrap();

        let cycle = TaskId::new();
        assert!(matches!(
            graph.add(node(cycle, vec![cycle], Budget::default())),
            Err(GraphError::Cycle(id)) if id == cycle
        ));
        let events = store.read(session, 1, 64).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "task.cycle_detected"));
        assert!(events
            .iter()
            .any(|event| event.kind == "task.budget_exhausted"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "task.attempt_finished")
                .count(),
            1,
            "idempotent completion appends once"
        );

        let rebuilt = open(&store, session);
        let node = rebuilt.node(first).unwrap();
        assert_eq!(node.state, TaskState::Completed);
        assert_eq!(node.runtime.used, budget(4), "usage survives a rebuild");
        assert_eq!(node.available(), budget(6));
        let attempts = rebuilt.attempts_of(first);
        assert_eq!(attempts.len(), 2, "the expired try is still on the record");
        assert_eq!(attempts[0].state, AttemptState::Expired);
        assert_eq!(attempts[1].state, AttemptState::Completed);
        assert_eq!(attempts[1].retry_of, Some(attempts[0].id));
        assert_eq!(attempts[1].used, budget(4));
        assert_eq!(attempts[1].lease_epoch, 2);
    }

    #[test]
    fn a_superseded_attempt_cannot_overwrite_the_retry_that_replaced_it() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let id = TaskId::new();
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        let first = graph.lease(id, AgentId::new(), 5).unwrap();
        graph.recover_expired(5).unwrap();
        let retry = graph.lease(id, AgentId::new(), 50).unwrap();

        assert!(matches!(
            graph.finish_attempt(
                first,
                &AttemptOutcome::completed(budget(1), json!({"late": true}), 6)
            ),
            Err(GraphError::FencedAttempt(_))
        ));
        graph
            .finish_attempt(
                retry,
                &AttemptOutcome::completed(budget(2), json!({"answer": "ok"}), 7),
            )
            .unwrap();
        assert!(matches!(
            graph.finish_attempt(retry, &AttemptOutcome::failed(budget(0), "duplicate", 8)),
            Err(GraphError::FencedAttempt(_))
        ));

        let rebuilt = open(&store, session);
        assert_eq!(rebuilt.node(id).unwrap().state, TaskState::Completed);
        assert_eq!(
            rebuilt.attempt(retry).unwrap().result,
            Some(json!({"answer": "ok"}))
        );
        assert_eq!(rebuilt.attempt(first).unwrap().state, AttemptState::Expired);
        assert_eq!(rebuilt.node(id).unwrap().runtime.used, budget(2));
    }

    #[test]
    fn concurrent_child_reservations_cannot_exceed_the_parent_budget() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let parent_id = TaskId::new();
        let mut owner = open(&store, session);
        owner.add(node(parent_id, Vec::new(), budget(4))).unwrap();

        // Four writers that each looked at the parent before any of them
        // reserved: without revalidation after contention, all five would fit.
        let mut writers: Vec<TaskGraph> = (0..5).map(|_| open(&store, session)).collect();
        let accepted = writers
            .iter_mut()
            .map(|writer| {
                let mut child = node(TaskId::new(), Vec::new(), budget(1));
                child.assignee = Some(AgentId::new());
                writer.add_child(parent_id, child, Vec::new())
            })
            .filter(Result::is_ok)
            .count();
        assert_eq!(
            accepted, 4,
            "the fifth reservation has nothing left to take"
        );

        let rebuilt = open(&store, session);
        let parent = rebuilt.node(parent_id).unwrap();
        assert_eq!(parent.runtime.reserved, budget(4));
        assert_eq!(parent.available(), Budget::default());
    }

    #[test]
    fn a_settled_child_returns_what_it_did_not_spend() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let parent_id = TaskId::new();
        let child_id = TaskId::new();
        let mut graph = open(&store, session);
        graph.add(node(parent_id, Vec::new(), budget(10))).unwrap();
        let mut child = node(child_id, Vec::new(), budget(6));
        child.assignee = Some(AgentId::new());
        graph.add_child(parent_id, child, Vec::new()).unwrap();
        assert_eq!(graph.node(parent_id).unwrap().available(), budget(4));

        graph.ready().unwrap();
        let attempt = graph.lease(child_id, AgentId::new(), 100).unwrap();
        graph.consume(child_id, budget(2)).unwrap();
        graph
            .finish_attempt(
                attempt,
                &AttemptOutcome::completed(Budget::default(), json!({"done": true}), 5),
            )
            .unwrap();

        let rebuilt = open(&store, session);
        let parent = rebuilt.node(parent_id).unwrap();
        assert_eq!(parent.runtime.reserved, Budget::default());
        assert_eq!(parent.runtime.used, budget(2), "only the spend settles");
        assert_eq!(parent.available(), budget(8));
    }

    #[test]
    fn execution_completion_is_not_verification() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let id = TaskId::new();
        let mut graph = open(&store, session);
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph.ready().unwrap();
        graph.lease(id, AgentId::new(), 100).unwrap();
        graph.complete(id, json!({"answer": "done"})).unwrap();
        assert_eq!(graph.node(id).unwrap().state, TaskState::Completed);

        // A sentence about a check is not a proof, whatever it says.
        assert!(matches!(
            graph.verify(id, json!({"validation": "cargo test"})),
            Err(GraphError::NotAProof(_, _))
        ));
        // Nor is a proof of some other task, nor one that is not verified.
        assert!(matches!(
            graph.verify(id, json!({"task": TaskId::new(), "state": "verified"})),
            Err(GraphError::NotAProof(_, _))
        ));
        assert!(matches!(
            graph.verify(id, json!({"task": id, "state": "partially_verified"})),
            Err(GraphError::NotAProof(_, _))
        ));

        graph
            .verify(id, json!({"task": id, "state": "verified"}))
            .unwrap();
        assert_eq!(graph.node(id).unwrap().state, TaskState::Verified);

        let mut unverifiable = open(&store, session);
        let other = TaskId::new();
        unverifiable
            .add(node(other, Vec::new(), budget(1)))
            .unwrap();
        // Even a well-formed proof cannot verify work that never ran.
        assert!(matches!(
            unverifiable.verify(other, json!({"task": other, "state": "verified"})),
            Err(GraphError::InvalidTransition(
                TaskState::Pending,
                TaskState::Verified
            ))
        ));
    }

    #[test]
    fn recovered_authority_is_narrowed_by_current_policy_and_never_widened() {
        use crate::{
            capability::{PolicySource, ResourcePattern},
            domain::GrantId,
        };
        let grant = |action, glob: &str, depth| CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action,
            scope: ResourceScope::single(ResourcePattern::new("file", glob).unwrap()),
            expires_at_ms: None,
            delegation_depth: depth,
            source: PolicySource::User,
        };
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let id = TaskId::new();
        let mut graph = open(&store, session);
        graph.add(node(id, Vec::new(), budget(10))).unwrap();
        graph
            .authorize(
                id,
                vec![
                    grant(CapabilityAction::FsRead, "/repo/**", 2),
                    grant(CapabilityAction::FsWrite, "/repo/**", 2),
                ],
            )
            .unwrap();

        // Policy today reads a narrower tree, delegates less far, and no
        // longer allows writes at all. It also allows a process the task never
        // held, which recovery must not hand it.
        let narrowed = graph
            .narrow_authority(
                id,
                &[
                    grant(CapabilityAction::FsRead, "/repo/src/**", 1),
                    grant(CapabilityAction::ProcessExec, "/repo/**", 3),
                ],
            )
            .unwrap();
        assert_eq!(narrowed.len(), 1, "the dropped action is not recovered");
        assert_eq!(narrowed[0].action, CapabilityAction::FsRead);
        assert_eq!(narrowed[0].delegation_depth, 1);
        assert_eq!(narrowed[0].scope.patterns().len(), 2, "both bounds apply");
        assert!(!narrowed[0]
            .scope
            .admits(&crate::domain::ResourceRef::new("file", "/repo/docs/a.md").unwrap()));

        let rebuilt = open(&store, session);
        assert_eq!(rebuilt.node(id).unwrap().authority, narrowed);
    }

    #[test]
    fn child_budget_and_authority_are_derived_from_the_parent() {
        use crate::{
            capability::{PolicySource, ResourcePattern},
            domain::GrantId,
        };
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = TaskGraph::new(store, SessionId::new(), Principal::System).unwrap();
        let parent_id = TaskId::new();
        let scope = ResourceScope::single(ResourcePattern::new("file", "/repo/**").unwrap());
        let mut parent = node(parent_id, Vec::new(), budget(10));
        parent.authority.push(CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action: CapabilityAction::FsRead,
            scope: scope.clone(),
            expires_at_ms: None,
            delegation_depth: 1,
            source: PolicySource::User,
        });
        graph.add(parent).unwrap();
        let child_id = TaskId::new();
        let mut child = node(child_id, vec![parent_id], budget(5));
        child.assignee = Some(AgentId::new());
        graph
            .add_child(
                parent_id,
                child,
                vec![ChildCapabilityRequest {
                    parent_grant: 0,
                    action: CapabilityAction::FsRead,
                    scope,
                    expires_at_ms: None,
                }],
            )
            .unwrap();
        let child = graph.node(child_id).unwrap();
        assert_eq!(child.runtime.parent, Some(parent_id));
        assert_eq!(child.authority.len(), 1);
    }

    #[test]
    fn retryable_failure_keeps_descendants_pending_until_retry_succeeds() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, child, grandchild] = std::array::from_fn(|_| TaskId::new());
        for (id, dependencies) in [
            (root, vec![]),
            (child, vec![root]),
            (grandchild, vec![child]),
        ] {
            graph.add(node(id, dependencies, budget(10))).unwrap();
        }
        graph.ready().unwrap();
        let agent = AgentId::new();
        let first = graph.lease(root, agent, 100).unwrap();
        graph
            .finish_attempt(
                first,
                &AttemptOutcome {
                    state: AttemptState::Failed,
                    used: budget(2),
                    reason: Some("transport interrupted".into()),
                    retryable: Retryability::Retryable,
                    result: None,
                    evidence: Vec::new(),
                    ended_at_ms: 20,
                },
            )
            .unwrap();
        assert_eq!(graph.node(child).unwrap().state, TaskState::Pending);
        assert_eq!(graph.node(grandchild).unwrap().state, TaskState::Pending);
        assert!(graph.block_unreachable().unwrap().is_empty());
        let before = store.current_version(session).unwrap();
        let mut rebuilt = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), before);
        assert_eq!(rebuilt.node(child).unwrap().state, TaskState::Pending);
        let retry = rebuilt
            .retry(
                root,
                &AttemptRequest {
                    role: "task".into(),
                    assignee: agent,
                    model: None,
                    base_revision: None,
                    started_at_ms: 30,
                    lease_expires_at_ms: 100,
                },
                2,
            )
            .unwrap();
        rebuilt
            .finish_attempt(
                retry,
                &AttemptOutcome::completed(Budget::default(), json!("done"), 40),
            )
            .unwrap();
        assert_eq!(rebuilt.ready().unwrap(), vec![child]);
        assert_eq!(rebuilt.node(grandchild).unwrap().state, TaskState::Pending);
    }

    #[test]
    fn scripted_plan_marker_is_idempotent_and_replays_without_writing() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [scripted, interactive] = std::array::from_fn(|_| TaskId::new());
        graph.add(node(scripted, vec![], budget(10))).unwrap();
        graph.add(node(interactive, vec![], budget(10))).unwrap();
        graph.mark_scripted_plan(scripted).unwrap();
        let version = store.current_version(session).unwrap();
        graph.mark_scripted_plan(scripted).unwrap();
        assert_eq!(store.current_version(session).unwrap(), version);
        let replayed = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), version);
        assert!(replayed.is_scripted_plan_authorized(scripted));
        assert!(!replayed.is_scripted_plan_authorized(interactive));
    }

    #[test]
    fn a_failed_attempt_blocks_every_descendant_without_scheduler_intervention() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, left, right, middle, join, leaf, independent] =
            std::array::from_fn(|_| TaskId::new());
        for (id, dependencies) in [
            (root, vec![]),
            (left, vec![root]),
            (right, vec![root]),
            (middle, vec![left]),
            (join, vec![middle, right]),
            (leaf, vec![join]),
            (independent, vec![]),
        ] {
            graph.add(node(id, dependencies, budget(10))).unwrap();
        }
        graph.ready().unwrap();
        let attempt = graph.lease(root, AgentId::new(), 100).unwrap();
        graph
            .finish_attempt(
                attempt,
                &AttemptOutcome {
                    state: AttemptState::Failed,
                    used: budget(2),
                    reason: Some("no result".into()),
                    retryable: Retryability::NotRetryable,
                    result: None,
                    evidence: Vec::new(),
                    ended_at_ms: 20,
                },
            )
            .unwrap();

        for id in [left, right, middle, join, leaf] {
            assert_eq!(graph.node(id).unwrap().state, TaskState::Blocked);
        }
        assert_eq!(graph.node(root).unwrap().state, TaskState::Failed);
        assert_eq!(
            graph
                .runnable()
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            vec![independent]
        );
        let events = store.read(session, 1, 64).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.kind == "task.transitioned"
                        && matches!(
                            &event.payload,
                            EventPayload::Inline { data } if data.get("to") == Some(&json!("blocked"))
                        )
                })
                .count(),
            5,
            "one durable blockage event per descendant"
        );
        let before = store.current_version(session).unwrap();
        assert!(graph.block_unreachable().unwrap().is_empty());
        let rebuilt = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), before);
        for id in [root, left, right, middle, join, leaf, independent] {
            assert_eq!(rebuilt.node(id).unwrap(), graph.node(id).unwrap());
        }
    }

    #[test]
    fn a_direct_cancel_blocks_pending_descendants_and_late_dependents() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, child, grandchild, later, completed] = std::array::from_fn(|_| TaskId::new());
        graph.add(node(root, vec![], budget(10))).unwrap();
        graph.add(node(child, vec![root], budget(10))).unwrap();
        graph
            .add(node(grandchild, vec![child], budget(10)))
            .unwrap();
        graph.add(node(completed, vec![], budget(10))).unwrap();
        graph.ready().unwrap();
        graph.lease(completed, AgentId::new(), 100).unwrap();
        graph.complete(completed, json!("done")).unwrap();
        graph.cancel(root, "operator").unwrap();
        graph
            .add(node(later, vec![grandchild], budget(10)))
            .unwrap();
        for id in [child, grandchild, later] {
            assert_eq!(graph.node(id).unwrap().state, TaskState::Blocked);
        }
        assert!(matches!(
            graph.cancel(completed, "too late"),
            Err(GraphError::InvalidTransition(
                TaskState::Completed,
                TaskState::Cancelled
            ))
        ));
        let before = store.current_version(session).unwrap();
        graph.cancel(root, "duplicate").unwrap();
        let rebuilt = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), before);
        assert_eq!(rebuilt.node(completed).unwrap().state, TaskState::Completed);
        for id in [root, child, grandchild, later] {
            assert_eq!(rebuilt.node(id).unwrap(), graph.node(id).unwrap());
        }
    }

    #[test]
    fn a_direct_fail_blocks_a_chain_even_for_a_stale_graph_view() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, child, grandchild, great_grandchild] = std::array::from_fn(|_| TaskId::new());
        for (id, dependencies) in [
            (root, vec![]),
            (child, vec![root]),
            (grandchild, vec![child]),
            (great_grandchild, vec![grandchild]),
        ] {
            graph.add(node(id, dependencies, budget(10))).unwrap();
        }
        let mut stale = open(&store, session);
        graph.fail(root, json!({"reason": "rejected"})).unwrap();
        let before = store.current_version(session).unwrap();
        stale.fail(root, json!({"reason": "duplicate"})).unwrap();
        assert_eq!(store.current_version(session).unwrap(), before);
        for id in [child, grandchild, great_grandchild] {
            assert_eq!(stale.node(id).unwrap().state, TaskState::Blocked);
        }
        assert!(stale.ready().unwrap().is_empty());
        assert!(stale.runnable().is_empty());
    }

    #[test]
    fn restart_repairs_an_older_failed_stream_without_duplicate_transitions() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, child, grandchild, great_grandchild] = std::array::from_fn(|_| TaskId::new());
        for (id, dependencies) in [
            (root, vec![]),
            (child, vec![root]),
            (grandchild, vec![child]),
            (great_grandchild, vec![grandchild]),
        ] {
            graph.add(node(id, dependencies, budget(10))).unwrap();
        }
        graph.ready().unwrap();
        let version = store.current_version(session).unwrap();
        let prior = store.read(session, version.0, 1).unwrap()[0].id;
        let legacy = EventEnvelope::new(
            session,
            version.0 + 1,
            Principal::System,
            Some(prior),
            CorrelationId::new(),
            SchemaVersion(1),
            "task.transitioned",
            EventPayload::Inline {
                data: json!({"task_id": root, "from": TaskState::Ready, "to": TaskState::Failed, "evidence": {"reason": "legacy"}}),
            },
        );
        store.append(session, version, vec![legacy]).unwrap();
        let mut rebuilt = open(&store, session);
        assert_eq!(
            store.current_version(session).unwrap().0,
            version.0 + 1,
            "opening a graph must not write to the event stream"
        );
        rebuilt.refresh().unwrap();
        assert_eq!(
            store.current_version(session).unwrap().0,
            version.0 + 1,
            "refresh is a read-only projection"
        );
        for id in [child, grandchild, great_grandchild] {
            assert_eq!(rebuilt.node(id).unwrap().state, TaskState::Pending);
        }
        assert_eq!(
            rebuilt
                .block_unreachable()
                .unwrap()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([child, grandchild, great_grandchild])
        );
        let repaired = store.current_version(session).unwrap();
        let again = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), repaired);
        for id in [root, child, grandchild, great_grandchild] {
            assert_eq!(again.node(id).unwrap(), rebuilt.node(id).unwrap());
        }
    }

    #[test]
    fn plan_approval_is_atomic_idempotent_and_read_only_on_replay() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let [root, first, second, third, unrelated] = std::array::from_fn(|_| TaskId::new());
        graph.add(node(root, vec![], budget(20))).unwrap();
        for (id, dependencies, step, todo) in [
            (first, vec![], "step-1", "todo-1"),
            (second, vec![first], "step-2", "todo-2"),
            (third, vec![], "step-3", "todo-3"),
        ] {
            let mut planned = node(id, dependencies, budget(20));
            planned.workspace = WorkspaceRequirement::IsolatedWriter;
            planned.runtime.plan = Some(PlanTask {
                root: Some(root),
                step_id: step.into(),
                todo_id: todo.into(),
            });
            graph.add(planned).unwrap();
        }
        graph.add(node(unrelated, vec![], budget(20))).unwrap();
        assert!(graph.ready().unwrap().iter().all(|id| *id != first));
        assert_eq!(graph.node(first).unwrap().state, TaskState::Pending);
        assert!(matches!(
            graph.approve_plan(&[first, unrelated], Some("keep tests focused")),
            Err(GraphError::NotAPlanTask(id)) if id == unrelated
        ));
        assert!(
            !graph.is_plan_approved(first),
            "invalid batch approves nothing"
        );

        let mut stale = open(&store, session);
        let mut operator = graph.fork(Principal::User("operator".into())).unwrap();
        operator
            .approve_plan(&[first, second], Some("keep tests focused"))
            .unwrap();
        let version = store.current_version(session).unwrap();
        stale
            .approve_plan(&[second, first, first], Some("keep tests focused"))
            .unwrap();
        assert_eq!(store.current_version(session).unwrap(), version);
        assert!(matches!(
            stale.approve_plan(&[first], Some("rewrite project")),
            Err(GraphError::PlanApprovalConflict(id)) if id == first
        ));
        assert!(matches!(
            stale.approve_plan(&[third, first], Some("rewrite project")),
            Err(GraphError::PlanApprovalConflict(id)) if id == first
        ));
        assert!(!stale.is_plan_approved(third), "mixed batch stays atomic");
        assert!(matches!(
            stale.approve_plan(&[second], None),
            Err(GraphError::PlanApprovalConflict(id)) if id == second
        ));
        assert_eq!(store.current_version(session).unwrap(), version);
        assert_eq!(stale.plan_approval_note(first), Some("keep tests focused"));
        assert_eq!(
            stale.approved_plan_tasks().collect::<BTreeSet<_>>(),
            BTreeSet::from([first, second])
        );
        let rebuilt = open(&store, session);
        assert_eq!(store.current_version(session).unwrap(), version);
        assert!(rebuilt.is_plan_approved(first));
        assert!(rebuilt.is_plan_approved(second));
        assert!(!rebuilt.is_plan_approved(unrelated));
        assert!(!rebuilt.is_plan_approved(third));
        assert_eq!(
            rebuilt.plan_approval_note(first),
            Some("keep tests focused")
        );
        assert_eq!(
            rebuilt.plan_approval_note(second),
            Some("keep tests focused")
        );
        assert_eq!(rebuilt.plan_approval_note(unrelated), None);
        let mut rebuilt = rebuilt;
        rebuilt.refresh().unwrap();
        assert_eq!(store.current_version(session).unwrap(), version);
        assert_eq!(graph.ready().unwrap(), vec![first]);

        let approvals: Vec<_> = store
            .read(session, 1, 64)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "task.plan_approved")
            .collect();
        assert_eq!(approvals.len(), 1, "one event approves the whole batch");
        assert_eq!(approvals[0].actor, Principal::User("operator".into()));
    }

    #[test]
    fn legacy_task_without_plan_metadata_cannot_be_approved() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let id = TaskId::new();
        let mut old_node = serde_json::to_value(node(id, vec![], budget(10))).unwrap();
        old_node["runtime"].as_object_mut().unwrap().remove("plan");
        let event = EventEnvelope::new(
            session,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "task.created",
            EventPayload::Inline {
                data: json!({"node": old_node}),
            },
        );
        store
            .append(session, StreamVersion(0), vec![event])
            .unwrap();
        let mut graph = open(&store, session);
        assert_eq!(graph.node(id).unwrap().runtime.plan, None);
        assert!(matches!(
            graph.approve_plan(&[id], None),
            Err(GraphError::NotAPlanTask(found)) if found == id
        ));
        assert!(!graph.is_plan_approved(id));
        assert_eq!(store.current_version(session).unwrap(), StreamVersion(1));
    }

    #[test]
    fn legacy_plan_approval_without_note_remains_approved_without_constraint() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let root = TaskId::new();
        let id = TaskId::new();
        graph.add(node(root, vec![], budget(10))).unwrap();
        let mut planned = node(id, vec![], budget(10));
        planned.workspace = WorkspaceRequirement::IsolatedWriter;
        planned.runtime.plan = Some(PlanTask {
            root: Some(root),
            step_id: "step-1".into(),
            todo_id: "todo-1".into(),
        });
        graph.add(planned).unwrap();
        let version = store.current_version(session).unwrap();
        let prior = store.read(session, version.0, 1).unwrap()[0].id;
        let event = EventEnvelope::new(
            session,
            version.0 + 1,
            Principal::User("operator".into()),
            Some(prior),
            CorrelationId::new(),
            SchemaVersion(1),
            "task.plan_approved",
            EventPayload::Inline {
                data: json!({"task_ids": [id]}),
            },
        );
        store.append(session, version, vec![event]).unwrap();
        let mut restored = open(&store, session);
        let before = store.current_version(session).unwrap();
        assert!(restored.is_plan_approved(id));
        assert_eq!(restored.plan_approval_note(id), None);
        restored.approve_plan(&[id], None).unwrap();
        assert!(matches!(
            restored.approve_plan(&[id], Some("new constraint")),
            Err(GraphError::PlanApprovalConflict(found)) if found == id
        ));
        assert_eq!(store.current_version(session).unwrap(), before);
    }

    #[test]
    fn persisted_ready_plan_task_still_requires_approval_to_start() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = open(&store, session);
        let root = TaskId::new();
        let id = TaskId::new();
        graph.add(node(root, vec![], budget(10))).unwrap();
        let mut planned = node(id, vec![], budget(10));
        planned.runtime.plan = Some(PlanTask {
            root: Some(root),
            step_id: "step-1".into(),
            todo_id: "todo-1".into(),
        });
        graph.add(planned).unwrap();
        let version = store.current_version(session).unwrap();
        let prior = store.read(session, version.0, 1).unwrap()[0].id;
        let event = EventEnvelope::new(
            session,
            version.0 + 1,
            Principal::System,
            Some(prior),
            CorrelationId::new(),
            SchemaVersion(1),
            "task.transitioned",
            EventPayload::Inline {
                data: json!({"task_id": id, "from": TaskState::Pending, "to": TaskState::Ready, "evidence": null}),
            },
        );
        store.append(session, version, vec![event]).unwrap();
        graph.refresh().unwrap();
        let before = store.current_version(session).unwrap();
        assert!(graph.runnable().iter().all(|task| task.id != id));
        assert!(matches!(
            graph.lease(id, AgentId::new(), 100),
            Err(GraphError::PlanNotApproved(found)) if found == id
        ));
        assert_eq!(store.current_version(session).unwrap(), before);
    }
}
