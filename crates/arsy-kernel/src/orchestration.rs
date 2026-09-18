//! The task graph: what was delegated, to whom, under what authority, and
//! what each try of it actually spent.
//!
//! Everything here is a projection of one session's event stream. A restarted
//! process rebuilds task lineage, attempt state, authority derivation, budget
//! reserved and used, and terminal reasons by replaying that stream — there is
//! no second scheduler database to disagree with it.

use crate::{
    capability::{AttenuationError, CapabilityAction, CapabilityGrant, ResourceScope},
    domain::{AgentId, AttemptId, CorrelationId, Principal, SessionId, TaskId, WorkspaceVersion},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
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
    Running,
    Completed,
    Failed,
    Cancelled,
    /// The lease ran out, or the holder was found gone. The attempt keeps its
    /// evidence, but it can no longer commit a result.
    Expired,
}

impl AttemptState {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
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
            result: Some(result),
            evidence: Vec::new(),
            ended_at_ms,
        }
    }

    pub fn failed(used: Budget, reason: impl Into<String>, ended_at_ms: u64) -> Self {
        Self {
            state: AttemptState::Failed,
            used,
            reason: Some(reason.into()),
            result: None,
            evidence: Vec::new(),
            ended_at_ms,
        }
    }
}

/// Graph-owned bookkeeping for a task. Callers building a [`TaskNode`] leave
/// it at its default: everything in it is written by the graph itself.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskRuntime {
    pub parent: Option<TaskId>,
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
    nodes: BTreeMap<TaskId, TaskNode>,
    attempts: BTreeMap<AttemptId, TaskAttempt>,
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
            nodes: BTreeMap::new(),
            attempts: BTreeMap::new(),
        };
        graph.catch_up()?;
        Ok(graph)
    }

    pub fn add(&mut self, node: TaskNode) -> Result<(), GraphError> {
        let node = TaskNode {
            runtime: TaskRuntime {
                parent: node.runtime.parent,
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
        let ready: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| self.is_ready(node).then_some(*id))
            .collect();
        for id in &ready {
            self.transition(*id, TaskState::Ready, None)?;
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
                state: AttemptState::Running,
                terminal_reason: None,
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
        if !outcome.state.is_terminal() {
            return Err(GraphError::NotTerminal(outcome.state));
        }
        self.commit("task.attempt_finished", |graph| {
            let record = graph
                .attempts
                .get(&attempt)
                .ok_or(GraphError::UnknownAttempt(attempt))?;
            if record.state != AttemptState::Running {
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
            Ok(json!({
                "attempt_id": attempt,
                "task_id": record.task,
                "state": outcome.state,
                "used": outcome.used,
                "reason": outcome.reason,
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
    pub fn verify(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        if self
            .nodes
            .get(&id)
            .is_some_and(|node| node.state == TaskState::Verified)
        {
            return Ok(());
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

    fn is_ready(&self, node: &TaskNode) -> bool {
        node.state == TaskState::Pending
            && node.dependencies.iter().all(|dependency| {
                self.nodes.get(dependency).is_some_and(|node| {
                    matches!(node.state, TaskState::Completed | TaskState::Verified)
                })
            })
    }

    /// End a task, through its running attempt when it has one.
    fn close(&mut self, id: TaskId, target: TaskState, evidence: Value) -> Result<(), GraphError> {
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
        self.commit("task.transitioned", |graph| {
            let current = graph.nodes.get(&id).ok_or(GraphError::Unknown(id))?.state;
            if !transition_allowed(current, target) {
                return Err(GraphError::InvalidTransition(current, target));
            }
            Ok(json!({"task_id": id, "from": current, "to": target, "evidence": evidence}))
        })
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
        match event.kind.as_str() {
            "task.created" => self.replay_created(data),
            "task.transitioned" => self.replay_transitioned(data),
            "task.attempt_started" => self.replay_attempt_started(data),
            "task.attempt_finished" => self.replay_attempt_finished(data),
            "task.attempt_expired" => self.replay_attempt_expired(data),
            "task.authorized" | "task.authority_narrowed" => self.replay_authority(data),
            "task.budget_used" => self.replay_budget_used(data),
            _ => Ok(()),
        }
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
        attempt.result = data.get("result").cloned().filter(|value| !value.is_null());
        attempt.evidence = field(data, "evidence").unwrap_or_default();
        let task = attempt.task;
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
        for attempt in 0..COMMIT_ATTEMPTS {
            let payload = build(self)?;
            let sequence = self.version.0.checked_add(1).ok_or(GraphError::Overflow)?;
            let event = EventEnvelope::new(
                self.session,
                sequence,
                self.actor.clone(),
                None,
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
                    return self.replay(&event);
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
            | (TaskState::Running, TaskState::Completed | TaskState::Failed)
            | (TaskState::Completed, TaskState::Verified)
            | (_, TaskState::Cancelled)
    )
}

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
    /// A result from an attempt that is no longer the task's current one.
    FencedAttempt(AttemptId),
    NotTerminal(AttemptState),
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
    /// The fallback is unreachable through [`Display`](fmt::Display), which
    /// names these variants explicitly: adding one without giving it a message
    /// stops compiling there rather than reaching this arm.
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

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyAuthorized(id) => {
                write!(formatter, "task {id} already holds recorded authority")
            }
            Self::Duplicate(id) => write!(formatter, "task {id} already exists"),
            Self::Unknown(id) => write!(formatter, "task {id} does not exist"),
            Self::UnknownAttempt(id) => write!(formatter, "attempt {id} does not exist"),
            Self::FencedAttempt(id) => write!(
                formatter,
                "attempt {id} has been superseded and cannot commit a result"
            ),
            Self::NotTerminal(state) => write!(formatter, "{state:?} does not end an attempt"),
            Self::Cycle(id) => write!(formatter, "task {id} introduces a dependency cycle"),
            Self::InvalidTransition(from, to) => {
                write!(formatter, "invalid task transition {from:?} -> {to:?}")
            }
            Self::InvalidEvent(message) => write!(formatter, "invalid task event: {message}"),
            Self::Attenuation(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            fixed @ (Self::BudgetExhausted(_)
            | Self::BudgetExpansion
            | Self::MissingAssignee
            | Self::MissingParentGrant
            | Self::Overflow) => formatter.write_str(fixed.constant()),
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
        graph
            .verify(id, json!({"validation": "cargo test"}))
            .unwrap();
        assert_eq!(graph.node(id).unwrap().state, TaskState::Verified);

        let mut unverifiable = open(&store, session);
        let other = TaskId::new();
        unverifiable
            .add(node(other, Vec::new(), budget(1)))
            .unwrap();
        assert!(matches!(
            unverifiable.verify(other, json!({})),
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
}
