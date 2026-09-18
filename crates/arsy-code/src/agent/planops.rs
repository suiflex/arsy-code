//! `plan.*`: a structured task plan retained for the current process.
//!
//! A long task is a sequence of steps, and a model that only ever says what it
//! is doing right now loses the sequence the moment the transcript is
//! trimmed. This gives the model a small ordered list it can create, revise,
//! and re-read across turns in the same process — the plan is the one thing
//! [`budget`](super::budget) trimming is never allowed to make disappear,
//! because it lives here rather than in the transcript. It is not restored
//! after process restart; durable commitments belong to `todo.*`.
//!
//! One executor per kind, same shape as [`fsops`](super::fsops): the kind
//! decides the action, the schema decides what a call needs to say. Every
//! kind shares one `Arc<Mutex<PlanState>>`, so an `add` and the `update` that
//! follows it in the same turn see each other's effect.

use super::todoops::Journal;
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    todo::{TodoAuthor, TodoList},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub status: PlanStepStatus,
    /// The durable TODO this step became, once it was committed. A plan is
    /// revised freely and kept in the process; this is the one transition that
    /// puts part of it on the record, and it happens once per step.
    #[serde(default)]
    pub committed_as: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanSnapshot {
    pub steps: Vec<PlanStep>,
}

impl PlanSnapshot {
    /// The step being worked, or the first one that has not been.
    ///
    /// A model is supposed to mark one step `in_progress`, and does not always
    /// remember to. Falling back to the first pending step means a plan view
    /// still says where the work is rather than showing nothing at all.
    pub fn current(&self) -> Option<&PlanStep> {
        self.steps
            .iter()
            .find(|step| step.status == PlanStepStatus::InProgress)
            .or_else(|| {
                self.steps
                    .iter()
                    .find(|step| step.status == PlanStepStatus::Pending)
            })
    }

    pub fn completed(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count()
    }
}

/// The plan one workspace holds, shared by every `plan.*` kind.
#[derive(Default)]
pub struct PlanState {
    steps: Vec<PlanStep>,
    next_id: u64,
}

impl PlanState {
    pub fn snapshot(&self) -> PlanSnapshot {
        PlanSnapshot {
            steps: self.steps.clone(),
        }
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|step| step.id == id)
    }
}

/// A fresh, empty plan, with no lifetime beyond whoever holds the `Arc`. Used
/// by tests, which want a plan isolated to one case rather than shared with
/// every other test in the process.
pub fn state() -> Arc<Mutex<PlanState>> {
    Arc::new(Mutex::new(PlanState::default()))
}

/// Every (workspace, scope)'s plan, kept alive for the life of the process.
///
/// `operations::registry` is rebuilt once per turn — a fresh [`PlanState`]
/// there would forget the plan the moment the model asked its next question.
/// The plan is exactly the thing this tool exists to keep, so it is cached
/// here instead, and every turn's registry is handed the same one back. The
/// scope is part of the key, not just the workspace root, so a second task or
/// session working the same workspace gets its own plan rather than picking
/// up whatever the first one left.
type WorkspaceKey = (PathBuf, String);
static WORKSPACES: OnceLock<Mutex<HashMap<WorkspaceKey, Arc<Mutex<PlanState>>>>> = OnceLock::new();

/// The plan for one (workspace, scope) pair, shared across every registry
/// built for it in this process. Lost on restart, the same ceiling
/// `budget`'s in-memory transcript has; a resumed session's plan is not this
/// ticket's scope.
pub fn state_for(workspace_root: &Path, scope: &str) -> Arc<Mutex<PlanState>> {
    let workspaces = WORKSPACES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut workspaces = workspaces.lock().unwrap_or_else(|error| error.into_inner());
    workspaces
        .entry((workspace_root.to_path_buf(), scope.to_owned()))
        .or_insert_with(state)
        .clone()
}

/// Read one scope's plan without dispatching an operation.
///
/// A plan view is a question about the harness's own state, not a tool call: a
/// front end that had to go through `plan.list` would need a capability, a
/// policy decision, and an artifact write to draw a sidebar. This is the same
/// data the tool returns, read directly.
pub fn snapshot_for(workspace_root: &Path, scope: &str) -> PlanSnapshot {
    state_for(workspace_root, scope)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .snapshot()
}

/// What a plan call may ask for. Kept separate rather than one kind with an
/// `action` field so a dry run and a policy rule can tell them apart — the
/// same reason [`fsops::FileOperation`](super::fsops::FileOperation) is split.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanOperation {
    Add,
    Update,
    Remove,
    Reorder,
    List,
    /// Turn the plan into commitments on the durable checklist.
    Commit,
}

impl PlanOperation {
    pub const ALL: [Self; 6] = [
        Self::Add,
        Self::Update,
        Self::Remove,
        Self::Reorder,
        Self::List,
        Self::Commit,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Add => "plan.add",
            Self::Update => "plan.update",
            Self::Remove => "plan.remove",
            Self::Reorder => "plan.reorder",
            Self::List => "plan.list",
            Self::Commit => "plan.commit",
        }
    }

    const fn idempotency(self) -> Idempotency {
        match self {
            Self::List => Idempotency::Idempotent,
            Self::Add | Self::Update | Self::Remove | Self::Reorder | Self::Commit => {
                Idempotency::Effectful
            }
        }
    }

    /// A read cannot conflict with another read, the same reason `fs.read` is
    /// `Parallel`; a mutation has to see the plan a previous one left, the same
    /// reason `fs.write` is not.
    const fn concurrency(self) -> ConcurrencyRule {
        match self {
            Self::List => ConcurrencyRule::Parallel,
            Self::Add | Self::Update | Self::Remove | Self::Reorder | Self::Commit => {
                ConcurrencyRule::ExclusiveGlobal
            }
        }
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let array = |name: &str| (name.to_owned(), JsonType::Array);
        let (required, optional) = match self {
            Self::Add => (vec![string("description")], vec![string("after")]),
            Self::Update => (
                vec![string("id")],
                vec![string("status"), string("description")],
            ),
            Self::Remove => (vec![string("id")], Vec::new()),
            Self::Reorder => (vec![array("order")], Vec::new()),
            Self::List | Self::Commit => (Vec::new(), Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

pub struct PlanExecutor {
    operation: PlanOperation,
    contract: OperationContract,
    state: Arc<Mutex<PlanState>>,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    journal: Option<Journal>,
}

impl PlanExecutor {
    /// `journal` is where `plan.commit` writes. A turn without one is offered
    /// no `plan.commit` at all rather than one that always refuses: the plan
    /// stays the scratch list it is, and nothing pretends to commit.
    pub fn executors(
        state: &Arc<Mutex<PlanState>>,
        artifacts: &Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
        journal: Option<&Journal>,
    ) -> Vec<Arc<dyn OperationExecutor>> {
        PlanOperation::ALL
            .into_iter()
            .filter(|operation| journal.is_some() || *operation != PlanOperation::Commit)
            .map(|operation| {
                Arc::new(Self {
                    operation,
                    contract: OperationContract {
                        kind: OperationKind::new(operation.kind())
                            .expect("static operation kind is valid"),
                        input_schema: operation.schema(),
                        actions: vec![CapabilityAction::SystemModify],
                        idempotency: operation.idempotency(),
                        reversible: true,
                        concurrency: operation.concurrency(),
                    },
                    state: Arc::clone(state),
                    artifacts: Arc::clone(artifacts),
                    retain_until_ms,
                    journal: journal.cloned(),
                }) as Arc<dyn OperationExecutor>
            })
            .collect()
    }

    fn put(
        &self,
        value: &impl Serialize,
        creator: Principal,
    ) -> Result<ResourceRef, OperationError> {
        super::store(
            self.artifacts.as_ref(),
            value,
            creator,
            self.retain_until_ms,
        )
    }

    /// Write every step that is not yet a commitment to the durable checklist,
    /// and record which TODO each one became.
    ///
    /// Once per step: the TODO id stays on the step, so committing a plan that
    /// gained two steps adds those two rather than the whole list again.
    fn commit(&self, state: &mut PlanState) -> Result<(), OperationError> {
        let journal = self.journal.as_ref().ok_or_else(|| {
            OperationError::Execution(
                "this turn has no durable checklist to commit a plan to".into(),
            )
        })?;
        let mut list = TodoList::open(
            Arc::clone(&journal.store),
            journal.session,
            journal.actor.clone(),
        )
        .map_err(|error| OperationError::Execution(error.to_string()))?;
        let uncommitted: Vec<usize> = state
            .steps
            .iter()
            .enumerate()
            .filter_map(|(index, step)| step.committed_as.is_none().then_some(index))
            .collect();
        if uncommitted.is_empty() {
            return Err(OperationError::Execution(
                "every step of this plan is already a commitment".into(),
            ));
        }
        for index in uncommitted {
            let item = list
                .add(
                    &state.steps[index].description,
                    Vec::new(),
                    TodoAuthor::Model,
                )
                .map_err(|error| OperationError::Execution(error.to_string()))?;
            state.steps[index].committed_as = Some(item.id);
        }
        Ok(())
    }
}

fn status_of(value: &str) -> Result<PlanStepStatus, OperationError> {
    match value {
        "pending" => Ok(PlanStepStatus::Pending),
        "in_progress" => Ok(PlanStepStatus::InProgress),
        "completed" => Ok(PlanStepStatus::Completed),
        other => Err(OperationError::Execution(format!(
            "`{other}` is not a plan step status; use pending, in_progress, or completed"
        ))),
    }
}

impl OperationExecutor for PlanExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        let string = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| OperationError::Execution("plan state poisoned".into()))?;

        match self.operation {
            PlanOperation::Add => {
                let description = string("description");
                if description.is_empty() {
                    return Err(OperationError::Execution(
                        "a plan step needs a description".into(),
                    ));
                }
                state.next_id += 1;
                let id = format!("step-{}", state.next_id);
                let step = PlanStep {
                    id,
                    description,
                    status: PlanStepStatus::Pending,
                    committed_as: None,
                };
                match input.get("after").and_then(Value::as_str) {
                    Some(after) if !after.is_empty() => {
                        let position = state
                            .position(after)
                            .ok_or_else(|| OperationError::Execution(format!("no step {after}")))?;
                        state.steps.insert(position + 1, step);
                    }
                    _ => state.steps.push(step),
                }
            }
            PlanOperation::Update => {
                let id = string("id");
                let position = state
                    .position(&id)
                    .ok_or_else(|| OperationError::Execution(format!("no step {id}")))?;
                if let Some(status) = input.get("status").and_then(Value::as_str) {
                    state.steps[position].status = status_of(status)?;
                }
                if let Some(description) = input.get("description").and_then(Value::as_str) {
                    if !description.is_empty() {
                        state.steps[position].description = description.to_owned();
                    }
                }
            }
            PlanOperation::Remove => {
                let id = string("id");
                let position = state
                    .position(&id)
                    .ok_or_else(|| OperationError::Execution(format!("no step {id}")))?;
                state.steps.remove(position);
            }
            PlanOperation::Reorder => {
                let order: Vec<String> = input
                    .get("order")
                    .and_then(Value::as_array)
                    .ok_or_else(|| OperationError::Execution("`order` must be an array".into()))?
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default().to_owned())
                    .collect();
                let mut current: Vec<&str> =
                    state.steps.iter().map(|step| step.id.as_str()).collect();
                current.sort_unstable();
                let mut requested: Vec<&str> = order.iter().map(String::as_str).collect();
                requested.sort_unstable();
                if current != requested {
                    return Err(OperationError::Execution(
                        "`order` must name exactly the current steps, once each".into(),
                    ));
                }
                let mut reordered = Vec::with_capacity(state.steps.len());
                for id in &order {
                    let position = state.position(id).expect("checked above");
                    reordered.push(state.steps[position].clone());
                }
                state.steps = reordered;
            }
            PlanOperation::List => {}
            PlanOperation::Commit => self.commit(&mut state)?,
        }

        let snapshot = state.snapshot();
        drop(state);
        let value = self.put(&snapshot, request.actor.clone())?;

        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::SystemModify,
                resource: ResourceRef::new("system", "plan")
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::FileArtifactStore,
        domain::{OperationId, Principal},
    };

    fn setup() -> (
        tempfile::TempDir,
        Vec<Arc<dyn OperationExecutor>>,
        Arc<dyn ArtifactStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let executors = PlanExecutor::executors(&state(), &artifacts, 0, None);
        (dir, executors, artifacts)
    }

    /// One `plan.*` call, returning whatever the executor did — a snapshot
    /// read back from its artifact, or the refusal.
    fn try_call(
        executors: &[Arc<dyn OperationExecutor>],
        artifacts: &Arc<dyn ArtifactStore>,
        kind: &str,
        input: Value,
    ) -> Result<PlanSnapshot, OperationError> {
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == kind)
            .unwrap_or_else(|| panic!("no executor for {kind}"));
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input,
        };
        let outcome = executor.execute(&request, &[])?;
        let reference = outcome.value.expect("plan calls always return a snapshot");
        let id: arsy_kernel::domain::ArtifactId = reference.value().parse().unwrap();
        let bytes = artifacts
            .read(
                id,
                arsy_kernel::artifact::ArtifactReadLimits {
                    max_bytes: 1024 * 1024,
                    max_expansion_ratio: 1_000,
                },
            )
            .unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    fn call(
        executors: &[Arc<dyn OperationExecutor>],
        artifacts: &Arc<dyn ArtifactStore>,
        kind: &str,
        input: Value,
    ) -> PlanSnapshot {
        try_call(executors, artifacts, kind, input).unwrap()
    }

    /// The plan is scratch and the checklist is the record, so committing is
    /// the one transition between them: it happens on request, once per step,
    /// and the steps it wrote are durable while the plan's own edits were not.
    #[test]
    fn committing_a_plan_writes_each_step_once_to_the_durable_checklist() {
        use arsy_kernel::{
            domain::SessionId,
            event::{EventStore, MemoryEventStore},
            todo::TodoList,
        };

        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let journal = Journal {
            store: Arc::clone(&store),
            session,
            actor: Principal::System,
            task: None,
            attempt: None,
        };
        let executors = PlanExecutor::executors(&state(), &artifacts, 0, Some(&journal));

        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "write the fix"}),
        );
        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "run the suite"}),
        );
        let committed = call(&executors, &artifacts, "plan.commit", serde_json::json!({}));
        assert_eq!(
            committed
                .steps
                .iter()
                .filter_map(|step| step.committed_as.as_deref())
                .collect::<Vec<_>>(),
            vec!["todo-1", "todo-2"]
        );

        let list = TodoList::open(Arc::clone(&store), session, Principal::System).unwrap();
        let items = list.snapshot().items;
        assert_eq!(items.len(), 2, "the commitment outlives the plan");
        assert_eq!(items[0].text, "write the fix");

        assert!(
            try_call(&executors, &artifacts, "plan.commit", serde_json::json!({})).is_err(),
            "committing twice would duplicate the checklist"
        );

        let (_dir, without_journal, _artifacts) = setup();
        assert!(
            !without_journal
                .iter()
                .any(|executor| executor.contract().kind.as_str() == "plan.commit"),
            "a turn with no session is offered no commit at all"
        );
    }

    #[test]
    fn a_step_can_be_created_revised_and_reordered() {
        let (_dir, executors, artifacts) = setup();

        let snapshot = call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "write the fix"}),
        );
        assert_eq!(snapshot.steps.len(), 1);
        assert_eq!(snapshot.steps[0].status, PlanStepStatus::Pending);
        let first = snapshot.steps[0].id.clone();

        let snapshot = call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "run the tests"}),
        );
        let second = snapshot.steps[1].id.clone();

        let snapshot = call(
            &executors,
            &artifacts,
            "plan.update",
            serde_json::json!({"id": first, "status": "in_progress"}),
        );
        assert_eq!(snapshot.steps[0].status, PlanStepStatus::InProgress);

        let snapshot = call(
            &executors,
            &artifacts,
            "plan.reorder",
            serde_json::json!({"order": [second.clone(), first.clone()]}),
        );
        assert_eq!(snapshot.steps[0].id, second);
        assert_eq!(snapshot.steps[1].id, first);

        let snapshot = call(
            &executors,
            &artifacts,
            "plan.remove",
            serde_json::json!({"id": second}),
        );
        assert_eq!(snapshot.steps.len(), 1);
        assert_eq!(snapshot.steps[0].id, first);
    }

    #[test]
    fn updating_an_unknown_step_is_an_error() {
        let (_dir, executors, artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "plan.update")
            .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({"id": "step-9", "status": "completed"}),
        };
        assert!(executor.execute(&request, &[]).is_err());
        let _ = artifacts;
    }
}
