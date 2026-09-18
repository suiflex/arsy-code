//! `todo.*`: the checklist that outlives the turn.
//!
//! Same shape as [`planops`](super::planops) — one executor per kind, all
//! sharing one state — and deliberately a different list. The plan is what the
//! model is doing now and may rewrite freely; a TODO is a commitment that has
//! to still be there after the process restarts, which is why this one is
//! written to the session's event stream rather than held in memory.
//!
//! See [`arsy_kernel::todo`] for why the two are not merged, and why neither
//! is the orchestration task graph.

use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef, SessionId},
    event::EventStore,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    todo::{TodoAuthor, TodoError, TodoList, TodoStatus},
};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Where a registry's durable checklist is kept.
///
/// Passed in rather than opened here because the store is the session's, not
/// the workspace's: a registry built for a dry run or a bare tool call has no
/// session, and must offer no `todo.*` kind rather than invent a stream to
/// write to.
#[derive(Clone)]
pub struct Journal {
    pub store: Arc<dyn EventStore>,
    pub session: SessionId,
    pub actor: Principal,
    /// The task and attempt this turn is working under, when it has one.
    /// Evidence written during the turn is attributed to them, so a later
    /// reader can tell which try of which task a check belongs to.
    pub task: Option<arsy_kernel::domain::TaskId>,
    pub attempt: Option<arsy_kernel::domain::AttemptId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TodoOperation {
    Add,
    Update,
    Remove,
    Reorder,
    List,
}

impl TodoOperation {
    pub const ALL: [Self; 5] = [
        Self::Add,
        Self::Update,
        Self::Remove,
        Self::Reorder,
        Self::List,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Add => "todo.add",
            Self::Update => "todo.update",
            Self::Remove => "todo.remove",
            Self::Reorder => "todo.reorder",
            Self::List => "todo.list",
        }
    }

    const fn idempotency(self) -> Idempotency {
        match self {
            Self::List => Idempotency::Idempotent,
            _ => Idempotency::Effectful,
        }
    }

    /// Reading cannot conflict with reading; every change has to see what the
    /// previous one wrote, because both append to one stream.
    const fn concurrency(self) -> ConcurrencyRule {
        match self {
            Self::List => ConcurrencyRule::Parallel,
            _ => ConcurrencyRule::ExclusiveGlobal,
        }
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let array = |name: &str| (name.to_owned(), JsonType::Array);
        let (required, optional) = match self {
            Self::Add => (vec![string("text")], vec![array("depends_on")]),
            Self::Update => (vec![string("id")], vec![string("status"), string("text")]),
            Self::Remove => (vec![string("id")], Vec::new()),
            Self::Reorder => (vec![array("order")], Vec::new()),
            Self::List => (Vec::new(), Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

pub struct TodoExecutor {
    operation: TodoOperation,
    contract: OperationContract,
    todos: Arc<Mutex<TodoList>>,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl TodoExecutor {
    /// One executor per kind, or none when the registry has no session to
    /// write to.
    pub fn executors(
        journal: &Journal,
        artifacts: &Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Result<Vec<Arc<dyn OperationExecutor>>, TodoError> {
        let todos = Arc::new(Mutex::new(TodoList::open(
            Arc::clone(&journal.store),
            journal.session,
            journal.actor.clone(),
        )?));
        Ok(TodoOperation::ALL
            .into_iter()
            .map(|operation| {
                Arc::new(Self {
                    operation,
                    contract: OperationContract {
                        kind: OperationKind::new(operation.kind())
                            .expect("static operation kind is valid"),
                        input_schema: operation.schema(),
                        actions: vec![CapabilityAction::SystemModify],
                        idempotency: operation.idempotency(),
                        // Nothing here touches the workspace, and every change
                        // is recoverable from the stream that recorded it.
                        reversible: true,
                        concurrency: operation.concurrency(),
                    },
                    todos: Arc::clone(&todos),
                    artifacts: Arc::clone(artifacts),
                    retain_until_ms,
                }) as Arc<dyn OperationExecutor>
            })
            .collect())
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
}

fn failed(error: TodoError) -> OperationError {
    OperationError::Execution(error.to_string())
}

fn strings(input: &Value, key: &str) -> Vec<String> {
    input
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
}

impl OperationExecutor for TodoExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
        let mut todos = self
            .todos
            .lock()
            .map_err(|_| OperationError::Execution("the TODO list is poisoned".into()))?;

        match self.operation {
            TodoOperation::Add => {
                // Who asked matters: an operator's checklist item is a
                // requirement, and the model's is a note to itself. A reader
                // deciding what may be dropped needs to tell them apart.
                let author = match &request.actor {
                    Principal::User(_) => TodoAuthor::User,
                    _ => TodoAuthor::Model,
                };
                todos
                    .add(text("text"), strings(input, "depends_on"), author)
                    .map_err(failed)?;
            }
            TodoOperation::Update => {
                let status = input
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|value| {
                        TodoStatus::parse(value).ok_or_else(|| {
                            OperationError::Execution(format!(
                                "`{value}` is not a TODO status; use pending, in_progress, \
                                 completed, or cancelled"
                            ))
                        })
                    })
                    .transpose()?;
                todos
                    .update(
                        text("id"),
                        status,
                        input.get("text").and_then(Value::as_str),
                    )
                    .map_err(failed)?;
            }
            TodoOperation::Remove => {
                todos.cancel(text("id")).map_err(failed)?;
            }
            TodoOperation::Reorder => {
                todos.reorder(&strings(input, "order")).map_err(failed)?;
            }
            TodoOperation::List => {}
        }

        let snapshot = todos.snapshot();
        drop(todos);
        let value = self.put(&snapshot, request.actor.clone())?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::SystemModify,
                resource: ResourceRef::new("system", "todo")
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
        artifact::FileArtifactStore, domain::OperationId, event::MemoryEventStore,
        todo::TodoSnapshot,
    };

    struct Harness {
        _directory: tempfile::TempDir,
        executors: Vec<Arc<dyn OperationExecutor>>,
        artifacts: Arc<dyn ArtifactStore>,
        journal: Journal,
    }

    fn harness() -> Harness {
        let directory = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(directory.path().join("artifacts"), 0).unwrap());
        let journal = Journal {
            task: None,
            attempt: None,
            store: Arc::new(MemoryEventStore::default()),
            session: SessionId::new(),
            actor: Principal::System,
        };
        let executors = TodoExecutor::executors(&journal, &artifacts, 0).unwrap();
        Harness {
            _directory: directory,
            executors,
            artifacts,
            journal,
        }
    }

    impl Harness {
        fn call(&self, kind: &str, input: Value) -> Result<TodoSnapshot, OperationError> {
            self.call_as(kind, input, Principal::User("operator".into()))
        }

        fn call_as(
            &self,
            kind: &str,
            input: Value,
            actor: Principal,
        ) -> Result<TodoSnapshot, OperationError> {
            let executor = self
                .executors
                .iter()
                .find(|executor| executor.contract().kind.as_str() == kind)
                .unwrap_or_else(|| panic!("no executor for {kind}"));
            let request = OperationRequest {
                id: OperationId::new(),
                kind: executor.contract().kind.clone(),
                actor,
                requirements: Vec::new(),
                input,
            };
            let outcome = executor.execute(&request, &[])?;
            let id: arsy_kernel::domain::ArtifactId = outcome
                .value
                .expect("every todo call returns a snapshot")
                .value()
                .parse()
                .unwrap();
            let bytes = self
                .artifacts
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
    }

    #[test]
    fn a_todo_is_created_worked_finished_and_still_there_after_a_resume() {
        let harness = harness();
        let snapshot = harness
            .call("todo.add", serde_json::json!({"text": "write the fix"}))
            .unwrap();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].author, TodoAuthor::User);
        let first = snapshot.items[0].id.clone();

        let snapshot = harness
            .call_as(
                "todo.add",
                serde_json::json!({"text": "run the tests", "depends_on": [first.clone()]}),
                Principal::System,
            )
            .unwrap();
        let second = snapshot.items[1].id.clone();
        assert_eq!(
            snapshot.items[1].author,
            TodoAuthor::Model,
            "who asked is recorded, not assumed"
        );

        // The dependant cannot start until what it waits for is done.
        assert!(harness
            .call(
                "todo.update",
                serde_json::json!({"id": second, "status": "in_progress"})
            )
            .is_err());

        harness
            .call(
                "todo.update",
                serde_json::json!({"id": first, "status": "completed"}),
            )
            .unwrap();
        let snapshot = harness
            .call(
                "todo.update",
                serde_json::json!({"id": second, "status": "in_progress"}),
            )
            .unwrap();
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.current.as_deref(), Some(second.as_str()));

        // A second registry over the same session — what a resume builds —
        // reads the checklist back rather than starting an empty one.
        let resumed = Harness {
            executors: TodoExecutor::executors(&harness.journal, &harness.artifacts, 0).unwrap(),
            ..harness
        };
        let snapshot = resumed.call("todo.list", serde_json::json!({})).unwrap();
        assert_eq!(snapshot.total, 2);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.items[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn an_unknown_status_is_refused_with_the_statuses_that_exist() {
        let harness = harness();
        let snapshot = harness
            .call("todo.add", serde_json::json!({"text": "something"}))
            .unwrap();
        let error = harness
            .call(
                "todo.update",
                serde_json::json!({"id": snapshot.items[0].id, "status": "nearly"}),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("in_progress"),
            "the refusal lists what is allowed: {error}"
        );
    }
}
