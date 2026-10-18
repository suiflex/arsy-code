//! `plan.*`: scratch steps, with explicit durable commitment.
//!
//! Drafts live in memory; committed plans have a root-linked event intent,
//! checklist entries, and task graph nodes. A resumed planning root can replay
//! its commitment without replacing already assigned task or TODO identities.
//!
//! One executor per kind, same shape as [`fsops`](super::fsops): the kind
//! decides the action, the schema decides what a call needs to say. Every
//! kind shares one `Arc<Mutex<PlanState>>`, so an `add` and the `update` that
//! follows it in the same turn see each other's effect.

use super::{input_string, todoops::Journal};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef, TaskId},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    orchestration::{PlanTask, TaskGraph, TaskNode, TaskRuntime, TaskState, WorkspaceRequirement},
    todo::{TodoAuthor, TodoItem, TodoList, TodoStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
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
    /// Other plan steps that must finish first. These stay scratch until
    /// commit, when they become TODO and TaskGraph edges.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The durable TODO this step became. Committed steps cannot be revised.
    #[serde(default)]
    pub committed_as: Option<String>,
    #[serde(default)]
    pub committed_task: Option<TaskId>,
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
    /// The first planning task that contributed steps; later turns cannot replace it.
    origin: Option<TaskId>,
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
/// built for it in this process. Recover committed steps from the stream with
/// [`recover_for`] when a planning root resumes after a process restart.
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

/// Commit the operator's current scratch plan without routing an agent tool call.
pub fn commit_for(
    workspace_root: &Path,
    scope: &str,
    journal: &Journal,
) -> Result<PlanSnapshot, OperationError> {
    let shared = state_for(workspace_root, scope);
    let mut state = shared
        .lock()
        .map_err(|_| OperationError::Execution("plan state poisoned".into()))?;
    PlanExecutor::commit(&mut state, journal)?;
    Ok(state.snapshot())
}
/// Rebuild a planning root's commitment from the session stream. This only
/// reads: a caller may then call `commit_for` to finish an interrupted commit.
pub fn recover_for(
    workspace_root: &Path,
    scope: &str,
    journal: &Journal,
) -> Result<PlanSnapshot, OperationError> {
    let root = journal.task.ok_or_else(|| {
        OperationError::Execution("recovery requires the original planning task".into())
    })?;
    let graph = TaskGraph::new(
        Arc::clone(&journal.store),
        journal.session,
        journal.actor.clone(),
    )
    .map_err(|error| OperationError::Execution(error.to_string()))?;
    if graph.node(root).is_none() {
        return Err(OperationError::Execution(format!(
            "planning task {root} does not exist"
        )));
    }
    let list = TodoList::open(
        Arc::clone(&journal.store),
        journal.session,
        journal.actor.clone(),
    )
    .map_err(|error| OperationError::Execution(error.to_string()))?;
    let shared = state_for(workspace_root, scope);
    let mut state = shared
        .lock()
        .map_err(|_| OperationError::Execution("plan state poisoned".into()))?;
    restore_root(&mut state, root, &graph, &list)?;
    Ok(state.snapshot())
}

fn next_todo_id(list: &TodoList) -> String {
    let next = list
        .snapshot()
        .items
        .iter()
        .filter_map(|item| item.id.strip_prefix("todo-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    format!("todo-{next}")
}

fn restore_root(
    state: &mut PlanState,
    root: TaskId,
    graph: &TaskGraph,
    list: &TodoList,
) -> Result<(), OperationError> {
    if graph.node(root).is_none() {
        return Err(OperationError::Execution(format!(
            "planning task {root} does not exist"
        )));
    }
    if state.origin.is_some_and(|origin| origin != root) {
        return Err(OperationError::Execution(
            "this plan belongs to a different planning task".into(),
        ));
    }
    let nodes: Vec<_> = graph
        .tasks()
        .filter(|node| {
            node.runtime
                .plan
                .as_ref()
                .is_some_and(|plan| plan.root == Some(root))
        })
        .collect();
    let items = list.snapshot().items;
    let items_by_id: HashMap<&str, &TodoItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();
    let durable = if let Some(intent) = graph.plan_intent(root) {
        serde_json::from_value::<Vec<PlanStep>>(intent.clone()).map_err(|error| {
            OperationError::Execution(format!("invalid durable plan intent: {error}"))
        })?
    } else {
        legacy_plan_steps(root, graph, &nodes, &items_by_id)?
    };
    validate_durable_plan(root, graph, &nodes, &durable, &items_by_id)?;
    if state.steps.is_empty() {
        state.steps = durable;
    } else if state.steps.len() < durable.len() || state.steps[..durable.len()] != durable[..] {
        return Err(OperationError::Execution(
            "scratch plan differs from durable commitments; recover the original planning root instead".into(),
        ));
    }
    state.origin = Some(root);
    state.next_id = state.next_id.max(
        state
            .steps
            .iter()
            .filter_map(|step| step.id.strip_prefix("step-")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0),
    );
    Ok(())
}

fn legacy_plan_steps(
    root: TaskId,
    graph: &TaskGraph,
    nodes: &[&TaskNode],
    items_by_id: &HashMap<&str, &TodoItem>,
) -> Result<Vec<PlanStep>, OperationError> {
    let mut legacy = Vec::with_capacity(nodes.len());
    for node in nodes {
        let plan = node.runtime.plan.as_ref().expect("filtered above");
        let item = items_by_id.get(plan.todo_id.as_str()).ok_or_else(|| {
            OperationError::Execution(format!(
                "task {} references missing TODO {}; inspect session history",
                node.id, plan.todo_id
            ))
        })?;
        if item.text != node.goal {
            return Err(OperationError::Execution(format!(
                "committed step {} disagrees with TODO {}; inspect session history",
                plan.step_id, plan.todo_id
            )));
        }
        legacy.push(PlanStep {
            id: plan.step_id.clone(),
            description: node.goal.clone(),
            status: PlanStepStatus::Pending,
            depends_on: node
                .dependencies
                .iter()
                .map(|id| {
                    graph
                        .node(*id)
                        .and_then(|node| node.runtime.plan.as_ref())
                        .filter(|plan| plan.root == Some(root))
                        .map(|plan| plan.step_id.clone())
                        .ok_or_else(|| {
                            OperationError::Execution(
                                "committed plan has an unknown dependency; inspect session history"
                                    .into(),
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
            committed_as: Some(item.id.clone()),
            committed_task: Some(node.id),
        });
    }
    legacy.sort_by_key(|step| {
        step.id
            .strip_prefix("step-")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    });
    Ok(legacy)
}

fn validate_durable_plan(
    root: TaskId,
    graph: &TaskGraph,
    nodes: &[&TaskNode],
    steps: &[PlanStep],
    items_by_id: &HashMap<&str, &TodoItem>,
) -> Result<(), OperationError> {
    let steps_by_id: HashMap<&str, &PlanStep> =
        steps.iter().map(|step| (step.id.as_str(), step)).collect();
    if steps_by_id.len() != steps.len() {
        return Err(OperationError::Execution(
            "ambiguous durable plan identities; inspect session history".into(),
        ));
    }
    let mut seen_todos = HashSet::new();
    let mut seen_tasks = HashSet::new();
    for step in steps {
        let todo = step.committed_as.as_deref().ok_or_else(|| {
            OperationError::Execution(
                "durable step has no reserved TODO; inspect session history".into(),
            )
        })?;
        let task = step.committed_task.ok_or_else(|| {
            OperationError::Execution(
                "durable step has no reserved task; inspect session history".into(),
            )
        })?;
        if !seen_todos.insert(todo) || !seen_tasks.insert(task) {
            return Err(OperationError::Execution(
                "ambiguous durable plan identities; inspect session history".into(),
            ));
        }
        validate_committed_step(root, graph, step, &steps_by_id, items_by_id)?;
    }
    if nodes.iter().any(|node| !seen_tasks.contains(&node.id)) {
        return Err(OperationError::Execution(
            "planning root has tasks outside its durable plan; inspect session history".into(),
        ));
    }
    Ok(())
}

fn validate_committed_step(
    root: TaskId,
    graph: &TaskGraph,
    step: &PlanStep,
    steps_by_id: &HashMap<&str, &PlanStep>,
    items_by_id: &HashMap<&str, &TodoItem>,
) -> Result<(), OperationError> {
    let todo = step.committed_as.as_deref().expect("checked above");
    let task = step.committed_task.expect("checked above");
    let dependencies = step
        .depends_on
        .iter()
        .map(|id| {
            steps_by_id.get(id.as_str()).copied().ok_or_else(|| {
                OperationError::Execution(format!("durable plan dependency {id} is missing"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(item) = items_by_id.get(todo) {
        if graph.plan_intent(root).is_some() && graph.plan_todo_id(root, &step.id) != Some(todo) {
            return Err(OperationError::Execution(format!(
                "TODO {todo} has no durable link to step {}; inspect session history",
                step.id
            )));
        }
        if item.text != step.description.trim()
            || item.author != TodoAuthor::Model
            || item.depends_on
                != dependencies
                    .iter()
                    .map(|dependency| dependency.committed_as.clone().expect("checked above"))
                    .collect::<Vec<_>>()
        {
            return Err(OperationError::Execution(format!(
                "TODO {todo} disagrees with committed step {}; inspect session history",
                step.id
            )));
        }
    }
    if let Some(node) = graph.node(task) {
        if node.goal != step.description
            || node.runtime.plan.as_ref()
                != Some(&PlanTask {
                    root: Some(root),
                    step_id: step.id.clone(),
                    todo_id: todo.to_owned(),
                })
            || node.dependencies
                != dependencies
                    .iter()
                    .map(|dependency| dependency.committed_task.expect("checked above"))
                    .collect::<Vec<_>>()
        {
            return Err(OperationError::Execution(format!(
                "task {task} disagrees with committed step {}; inspect session history",
                step.id
            )));
        }
    }
    Ok(())
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
            Self::Add => (
                vec![string("description")],
                vec![string("after"), array("depends_on")],
            ),
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

    /// Reconcile the current scratch view against the durable root before
    /// adding anything, including when an earlier process died mid-commit.
    fn recover_root(
        state: &mut PlanState,
        root: TaskId,
        graph: &TaskGraph,
        list: &TodoList,
    ) -> Result<(), OperationError> {
        restore_root(state, root, graph, list)
    }

    /// Write every step that is not yet a commitment to the durable checklist,
    /// and record which TODO each one became.
    ///
    /// Once per step: the TODO id stays on the step, so committing a plan that
    /// gained two steps adds those two rather than the whole list again.
    fn commit(state: &mut PlanState, journal: &Journal) -> Result<(), OperationError> {
        let mut list = TodoList::open(
            Arc::clone(&journal.store),
            journal.session,
            journal.actor.clone(),
        )
        .map_err(|error| OperationError::Execution(error.to_string()))?;
        let root = state.origin.or(journal.task);
        let graph_required = root.is_some();
        let mut graph = graph_required
            .then(|| {
                TaskGraph::new(
                    Arc::clone(&journal.store),
                    journal.session,
                    journal.actor.clone(),
                )
            })
            .transpose()
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        if let Some(root) = root {
            Self::recover_root(
                state,
                root,
                graph.as_ref().expect("root requires graph"),
                &list,
            )?;
            let graph = graph.as_ref().expect("root requires graph");
            if graph.plan_intent(root).is_none()
                && graph.tasks().any(|node| {
                    node.runtime
                        .plan
                        .as_ref()
                        .is_some_and(|plan| plan.root == Some(root))
                })
                && state.steps.iter().any(|step| step.committed_as.is_none())
            {
                return Err(OperationError::Execution(
                    "older committed plan has no durable intent; finish this root and start a new planning task for additional steps".into(),
                ));
            }
        }
        if state.steps.is_empty() {
            return Err(OperationError::Execution(
                "a plan needs steps to commit".into(),
            ));
        }
        let by_id: HashMap<_, _> = state
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| (step.id.clone(), index))
            .collect();
        let order = dependency_order(&state.steps, &by_id)?;
        let budget = root
            .map(|id| {
                graph
                    .as_ref()
                    .and_then(|graph| graph.node(id))
                    .map(|node| node.budget)
                    .ok_or_else(|| {
                        OperationError::Execution(format!("planning task {id} does not exist"))
                    })
            })
            .transpose()?
            .unwrap_or_default();
        if root.is_some() && budget.tokens == 0 {
            return Err(OperationError::Execution(
                "planning task has no token budget for plan steps".into(),
            ));
        }

        // Reserve identities in a durable intent before either projection is
        // mutated. If the process dies, recovery knows *all* intended steps,
        // including those whose TODO or task creation has not yet happened.
        if let Some(root) = root {
            for &index in &order {
                if state.steps[index].committed_as.is_none() {
                    // The ordinary checklist only allocates `todo-N`. A
                    // root/step identity in a separate namespace cannot be
                    // claimed by that allocator, even across concurrent turns.
                    let todo_id = format!("plan-{root}-{}-todo", state.steps[index].id);
                    state.steps[index].committed_as = Some(todo_id);
                    state.steps[index].committed_task = Some(TaskId::new());
                }
            }
            let values = state
                .steps
                .iter()
                .map(|step| {
                    serde_json::to_value(step)
                        .map_err(|error| OperationError::Execution(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let graph = graph.as_mut().expect("root requires graph");
            if graph.plan_intent(root).is_some()
                || state.steps.iter().any(|step| {
                    step.committed_task.is_none()
                        || graph
                            .node(step.committed_task.expect("checked above"))
                            .is_none()
                })
            {
                graph
                    .record_plan_intent(root, &values)
                    .map_err(|error| OperationError::Execution(error.to_string()))?;
            }
        }

        for &index in &order {
            let step = &state.steps[index];
            if let Some(graph) = graph.as_mut() {
                let exists = step
                    .committed_task
                    .is_some_and(|task| graph.node(task).is_some());
                if !exists {
                    let dependencies = step
                        .depends_on
                        .iter()
                        .map(|dependency| {
                            by_id
                                .get(dependency)
                                .and_then(|index| state.steps[*index].committed_task)
                                .ok_or_else(|| {
                                    OperationError::Execution(format!(
                                        "step {} depends on uncompiled {dependency}",
                                        step.id
                                    ))
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let id = step.committed_task.unwrap_or_default();
                    graph
                        .add(TaskNode {
                            id,
                            goal: step.description.clone(),
                            dependencies,
                            assignee: None,
                            required_output: "text".into(),
                            workspace: WorkspaceRequirement::IsolatedWriter,
                            budget,
                            authority: Vec::new(),
                            state: TaskState::Pending,
                            lease_expires_at_ms: None,
                            runtime: TaskRuntime {
                                plan: Some(PlanTask {
                                    root,
                                    step_id: step.id.clone(),
                                    todo_id: step
                                        .committed_as
                                        .clone()
                                        .unwrap_or_else(|| next_todo_id(&list)),
                                }),
                                ..TaskRuntime::default()
                            },
                        })
                        .map_err(|error| OperationError::Execution(error.to_string()))?;
                    state.steps[index].committed_task = Some(id);
                }
            }
            if let Some(expected) = &state.steps[index].committed_as {
                if list
                    .snapshot()
                    .items
                    .iter()
                    .any(|item| item.id == *expected)
                {
                    continue;
                }
            }
            let dependencies = state.steps[index]
                .depends_on
                .iter()
                .map(|dependency| {
                    by_id
                        .get(dependency)
                        .and_then(|index| state.steps[*index].committed_as.clone())
                        .ok_or_else(|| {
                            OperationError::Execution(format!(
                                "step {} depends on uncommitted {dependency}",
                                state.steps[index].id
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let step = &state.steps[index];
            if let Some(root) = root {
                let graph = graph.as_mut().expect("root requires graph");
                let item = TodoItem {
                    id: step.committed_as.clone().expect("reserved in intent"),
                    text: step.description.trim().to_owned(),
                    status: TodoStatus::Pending,
                    depends_on: dependencies,
                    author: TodoAuthor::Model,
                };
                graph
                    .record_plan_todo(root, &step.id, &item)
                    .map_err(|error| OperationError::Execution(error.to_string()))?;
                list = TodoList::open(
                    Arc::clone(&journal.store),
                    journal.session,
                    journal.actor.clone(),
                )
                .map_err(|error| OperationError::Execution(error.to_string()))?;
            } else {
                let item = list
                    .add(&step.description, dependencies, TodoAuthor::Model)
                    .map_err(|error| OperationError::Execution(error.to_string()))?;
                state.steps[index].committed_as = Some(item.id);
            }
        }
        Ok(())
    }
}

fn dependency_order(
    steps: &[PlanStep],
    by_id: &HashMap<String, usize>,
) -> Result<Vec<usize>, OperationError> {
    let mut marks = vec![0_u8; steps.len()];
    let mut order = Vec::with_capacity(steps.len());
    for index in 0..steps.len() {
        visit_step(index, steps, by_id, &mut marks, &mut order)?;
    }
    Ok(order)
}

fn visit_step(
    index: usize,
    steps: &[PlanStep],
    by_id: &HashMap<String, usize>,
    marks: &mut [u8],
    order: &mut Vec<usize>,
) -> Result<(), OperationError> {
    match marks[index] {
        2 => return Ok(()),
        1 => {
            return Err(OperationError::Execution(
                "plan dependencies are cyclic".into(),
            ))
        }
        _ => marks[index] = 1,
    }
    for dependency in &steps[index].depends_on {
        let dependency = *by_id
            .get(dependency)
            .ok_or_else(|| OperationError::Execution(format!("no dependency step {dependency}")))?;
        visit_step(dependency, steps, by_id, marks, order)?;
    }
    marks[index] = 2;
    order.push(index);
    Ok(())
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

fn add_step(state: &mut PlanState, input: &Value) -> Result<(), OperationError> {
    let description = input_string(input, "description");
    if description.is_empty() {
        return Err(OperationError::Execution(
            "a plan step needs a description".into(),
        ));
    }
    let depends_on: Vec<String> = input
        .get("depends_on")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(step) = state.steps.iter().find(|step| {
        step.committed_as.is_some()
            && step.description == description
            && step.depends_on == depends_on
    }) {
        return Err(OperationError::Execution(format!(
            "step {} is already committed; use plan.list instead of adding it again",
            step.id
        )));
    }
    let next_id = state
        .next_id
        .checked_add(1)
        .ok_or_else(|| OperationError::Execution("plan step IDs exhausted".into()))?;
    let step = PlanStep {
        id: format!("step-{next_id}"),
        description,
        status: PlanStepStatus::Pending,
        depends_on,
        committed_as: None,
        committed_task: None,
    };
    if let Some(missing) = step
        .depends_on
        .iter()
        .find(|dependency| state.position(dependency).is_none())
    {
        return Err(OperationError::Execution(format!(
            "no dependency step {missing}"
        )));
    }
    let insertion = match input.get("after").and_then(Value::as_str) {
        Some(after) if !after.is_empty() => {
            let position = state
                .position(after)
                .ok_or_else(|| OperationError::Execution(format!("no step {after}")))?;
            if state.steps[position + 1..]
                .iter()
                .any(|step| step.committed_as.is_some())
            {
                return Err(OperationError::Execution(
                    "cannot insert before committed steps; append new work instead".into(),
                ));
            }
            Some(position + 1)
        }
        _ => None,
    };
    state.next_id = next_id;
    if let Some(position) = insertion {
        state.steps.insert(position, step);
    } else {
        state.steps.push(step);
    }
    Ok(())
}

fn update_step(state: &mut PlanState, input: &Value) -> Result<(), OperationError> {
    let id = input_string(input, "id");
    let position = state
        .position(&id)
        .ok_or_else(|| OperationError::Execution(format!("no step {id}")))?;
    if state.steps[position].committed_as.is_some() {
        return Err(OperationError::Execution(format!(
            "step {id} is committed; edit its durable TODO or create a new plan step"
        )));
    }
    if let Some(status) = input.get("status").and_then(Value::as_str) {
        state.steps[position].status = status_of(status)?;
    }
    if let Some(description) = input
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
    {
        state.steps[position].description = description.to_owned();
    }
    Ok(())
}

fn remove_step(state: &mut PlanState, input: &Value) -> Result<(), OperationError> {
    let id = input_string(input, "id");
    let position = state
        .position(&id)
        .ok_or_else(|| OperationError::Execution(format!("no step {id}")))?;
    if state.steps[position].committed_as.is_some() {
        return Err(OperationError::Execution(format!(
            "step {id} is committed; keep its durable TODO and task instead"
        )));
    }
    if state.steps.iter().any(|step| step.depends_on.contains(&id)) {
        return Err(OperationError::Execution(format!(
            "step {id} is a dependency; update dependent steps before removing it"
        )));
    }
    state.steps.remove(position);
    Ok(())
}

fn reorder_steps(state: &mut PlanState, input: &Value) -> Result<(), OperationError> {
    let order: Vec<String> = input
        .get("order")
        .and_then(Value::as_array)
        .ok_or_else(|| OperationError::Execution("`order` must be an array".into()))?
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_owned())
        .collect();
    let mut current: Vec<&str> = state.steps.iter().map(|step| step.id.as_str()).collect();
    current.sort_unstable();
    let mut requested: Vec<&str> = order.iter().map(String::as_str).collect();
    requested.sort_unstable();
    if current != requested {
        return Err(OperationError::Execution(
            "`order` must name exactly the current steps, once each".into(),
        ));
    }
    if state
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .eq(order.iter().map(String::as_str))
    {
        return Ok(());
    }
    if state.steps.iter().any(|step| step.committed_as.is_some()) {
        return Err(OperationError::Execution(
            "committed steps cannot be reordered; add new steps after them".into(),
        ));
    }
    let mut by_id: HashMap<_, _> = state
        .steps
        .drain(..)
        .map(|step| (step.id.clone(), step))
        .collect();
    state.steps = order
        .iter()
        .map(|id| by_id.remove(id).expect("checked above"))
        .collect();
    Ok(())
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| OperationError::Execution("plan state poisoned".into()))?;

        match self.operation {
            PlanOperation::Add => {
                let first_step = state.steps.is_empty();
                add_step(&mut state, input)?;
                if first_step {
                    state.origin = self.journal.as_ref().and_then(|journal| journal.task);
                }
            }
            PlanOperation::Update => update_step(&mut state, input)?,
            PlanOperation::Remove => remove_step(&mut state, input)?,
            PlanOperation::Reorder => reorder_steps(&mut state, input)?,
            PlanOperation::List => {}
            PlanOperation::Commit => Self::commit(
                &mut state,
                self.journal.as_ref().ok_or_else(|| {
                    OperationError::Execution("this turn has no durable checklist".into())
                })?,
            )?,
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
        orchestration::Budget,
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
            orchestration::TaskGraph,
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
            serde_json::json!({"description": "run the suite", "depends_on": ["step-1"]}),
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
        assert_eq!(items[1].depends_on, vec!["todo-1"]);

        let graph = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        assert_eq!(
            graph.tasks().count(),
            0,
            "without an execution root, commitments remain TODO-only"
        );
        let version = store.current_version(session).unwrap();
        let retried = call(&executors, &artifacts, "plan.commit", serde_json::json!({}));
        assert_eq!(retried, committed, "retry keeps the same TODO and task ids");
        assert_eq!(store.current_version(session).unwrap(), version);

        let (_dir, without_journal, _artifacts) = setup();
        assert!(
            !without_journal
                .iter()
                .any(|executor| executor.contract().kind.as_str() == "plan.commit"),
            "a turn with no session is offered no commit at all"
        );
    }

    #[test]
    fn operator_commit_uses_original_planning_root_and_replays_runnable_metadata() {
        use arsy_kernel::{
            domain::SessionId,
            event::{EventStore, MemoryEventStore},
        };

        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        let planning = TaskId::new();
        let later_turn = TaskId::new();
        let budget = Budget {
            tokens: 12_000,
            cost_micros: 750_000,
            wall_ms: 90_000,
        };
        for id in [planning, later_turn] {
            graph
                .add(TaskNode {
                    id,
                    goal: "planning".into(),
                    dependencies: Vec::new(),
                    assignee: None,
                    required_output: "text".into(),
                    workspace: WorkspaceRequirement::IsolatedWriter,
                    budget,
                    authority: Vec::new(),
                    state: TaskState::Pending,
                    lease_expires_at_ms: None,
                    runtime: TaskRuntime::default(),
                })
                .unwrap();
        }
        let journal = Journal {
            store: Arc::clone(&store),
            session,
            actor: Principal::System,
            task: Some(planning),
            attempt: None,
        };
        let shared = state_for(dir.path(), "operator-approval");
        let executors = PlanExecutor::executors(&shared, &artifacts, 0, Some(&journal));
        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "implement"}),
        );
        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "verify", "depends_on": ["step-1"]}),
        );
        let operator_journal = Journal {
            task: Some(later_turn),
            ..journal
        };
        let committed = commit_for(dir.path(), "operator-approval", &operator_journal).unwrap();
        let version = store.current_version(session).unwrap();
        assert_eq!(
            commit_for(dir.path(), "operator-approval", &operator_journal).unwrap(),
            committed
        );
        assert_eq!(store.current_version(session).unwrap(), version);
        let mut restored = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        let ids: Vec<_> = committed
            .steps
            .iter()
            .map(|step| step.committed_task.unwrap())
            .collect();
        for (step, id) in committed.steps.iter().zip(ids.iter().copied()) {
            let node = restored.node(id).unwrap();
            assert_eq!(node.runtime.parent, None, "step budgets are independent");
            assert_eq!(node.budget, budget);
            assert_eq!(node.workspace, WorkspaceRequirement::IsolatedWriter);
            assert!(node.authority.is_empty(), "the model grants no permissions");
            assert_eq!(
                node.runtime.plan,
                Some(PlanTask {
                    root: Some(planning),
                    step_id: step.id.clone(),
                    todo_id: step.committed_as.clone().unwrap(),
                })
            );
            assert!(!restored.is_plan_approved(id));
        }
        assert_eq!(restored.node(ids[1]).unwrap().dependencies, vec![ids[0]]);
        assert!(restored.ready().unwrap().iter().all(|id| !ids.contains(id)));
        assert_eq!(restored.node(ids[0]).unwrap().state, TaskState::Pending);
        assert_eq!(
            restored.plan_intent_roots().collect::<Vec<_>>(),
            vec![planning]
        );
        let replay_journal = Journal {
            task: Some(planning),
            ..operator_journal.clone()
        };
        let before = store.current_version(session).unwrap();
        let recovered = recover_for(dir.path(), "restarted-planner", &replay_journal).unwrap();
        assert_eq!(recovered, committed);
        assert_eq!(
            store.current_version(session).unwrap(),
            before,
            "replay never writes"
        );
        assert_eq!(
            commit_for(dir.path(), "restarted-planner", &replay_journal).unwrap(),
            committed
        );
        assert_eq!(store.current_version(session).unwrap(), before);

        let resumed = PlanExecutor::executors(
            &state_for(dir.path(), "restarted-planner"),
            &artifacts,
            0,
            Some(&replay_journal),
        );
        for (operation, input) in [
            ("plan.add", serde_json::json!({"description": "implement"})),
            (
                "plan.update",
                serde_json::json!({"id": "step-1", "description": "rewrite"}),
            ),
            ("plan.remove", serde_json::json!({"id": "step-1"})),
            (
                "plan.reorder",
                serde_json::json!({"order": ["step-2", "step-1"]}),
            ),
            (
                "plan.add",
                serde_json::json!({"description": "insert first", "after": "step-1"}),
            ),
        ] {
            assert!(
                try_call(&resumed, &artifacts, operation, input).is_err(),
                "{operation} cannot rewrite a committed dependency"
            );
        }
        assert_eq!(
            try_call(&resumed, &artifacts, "plan.list", serde_json::json!({})).unwrap(),
            committed
        );
        call(
            &resumed,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "document result", "depends_on": ["step-2"]}),
        );
        let extended = commit_for(dir.path(), "restarted-planner", &replay_journal).unwrap();
        assert_eq!(&extended.steps[..2], &committed.steps[..]);
        let expected_todo = format!("plan-{planning}-step-3-todo");
        assert_eq!(
            extended.steps[2].committed_as.as_deref(),
            Some(expected_todo.as_str())
        );
        assert_eq!(
            TaskGraph::new(store, session, Principal::System)
                .unwrap()
                .node(extended.steps[2].committed_task.unwrap())
                .unwrap()
                .dependencies,
            vec![ids[1]]
        );
    }

    #[test]
    fn interrupted_commit_reuses_reserved_ids_and_allows_independent_todos() {
        use arsy_kernel::{
            domain::SessionId,
            event::{EventStore, MemoryEventStore},
        };

        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let root = TaskId::new();
        let mut graph = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        let budget = Budget {
            tokens: 500,
            cost_micros: 1_000,
            wall_ms: 2_000,
        };
        graph
            .add(TaskNode {
                id: root,
                goal: "planning".into(),
                dependencies: Vec::new(),
                assignee: None,
                required_output: "text".into(),
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget,
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: TaskRuntime::default(),
            })
            .unwrap();
        let [first, second] = std::array::from_fn(|_| TaskId::new());
        let first_todo = format!("plan-{root}-step-1-todo");
        let second_todo = format!("plan-{root}-step-2-todo");
        let steps = vec![
            PlanStep {
                id: "step-1".into(),
                description: "implement".into(),
                status: PlanStepStatus::Pending,
                depends_on: Vec::new(),
                committed_as: Some(first_todo.clone()),
                committed_task: Some(first),
            },
            PlanStep {
                id: "step-2".into(),
                description: "verify".into(),
                status: PlanStepStatus::Pending,
                depends_on: vec!["step-1".into()],
                committed_as: Some(second_todo),
                committed_task: Some(second),
            },
        ];
        graph
            .record_plan_intent(
                root,
                &steps
                    .iter()
                    .map(|step| serde_json::to_value(step).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        graph
            .add(TaskNode {
                id: first,
                goal: "implement".into(),
                dependencies: Vec::new(),
                assignee: None,
                required_output: "text".into(),
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget,
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: TaskRuntime {
                    plan: Some(PlanTask {
                        root: Some(root),
                        step_id: "step-1".into(),
                        todo_id: first_todo.clone(),
                    }),
                    ..TaskRuntime::default()
                },
            })
            .unwrap();
        graph
            .record_plan_todo(
                root,
                "step-1",
                &TodoItem {
                    id: first_todo.clone(),
                    text: "implement".into(),
                    status: TodoStatus::Pending,
                    depends_on: Vec::new(),
                    author: TodoAuthor::Model,
                },
            )
            .unwrap();
        let journal = Journal {
            store: Arc::clone(&store),
            session,
            actor: Principal::System,
            task: Some(root),
            attempt: None,
        };
        let version = store.current_version(session).unwrap();
        let recovered = recover_for(dir.path(), "interrupted", &journal).unwrap();
        assert_eq!(recovered.steps, steps);
        assert_eq!(store.current_version(session).unwrap(), version);
        let complete = commit_for(dir.path(), "interrupted", &journal).unwrap();
        assert_eq!(complete.steps, steps);
        let final_version = store.current_version(session).unwrap();
        assert_eq!(
            commit_for(dir.path(), "interrupted", &journal).unwrap(),
            complete
        );
        assert_eq!(store.current_version(session).unwrap(), final_version);
        let rebuilt = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        assert_eq!(rebuilt.node(second).unwrap().dependencies, vec![first]);
        assert_eq!(
            TodoList::open(Arc::clone(&store), session, Principal::System)
                .unwrap()
                .snapshot()
                .items[1]
                .depends_on,
            vec![first_todo]
        );

        let other = tempfile::tempdir().unwrap();
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let other_session = SessionId::new();
        let mut other_graph =
            TaskGraph::new(Arc::clone(&store), other_session, Principal::System).unwrap();
        other_graph
            .add(TaskNode {
                id: root,
                ..rebuilt.node(root).unwrap().clone()
            })
            .unwrap();
        other_graph
            .record_plan_intent(
                root,
                &steps
                    .iter()
                    .map(|step| serde_json::to_value(step).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        TodoList::open(Arc::clone(&store), other_session, Principal::System)
            .unwrap()
            .add("implement", Vec::new(), TodoAuthor::Model)
            .unwrap();
        let contested = Journal {
            store,
            session: other_session,
            ..journal
        };
        assert_eq!(
            recover_for(other.path(), "independent-todo", &contested)
                .unwrap()
                .steps,
            steps
        );
        assert_eq!(
            commit_for(other.path(), "independent-todo", &contested)
                .unwrap()
                .steps,
            steps
        );
        assert_eq!(
            TodoList::open(
                Arc::clone(&contested.store),
                other_session,
                Principal::System
            )
            .unwrap()
            .snapshot()
            .items
            .iter()
            .filter(|item| item.id == "todo-1")
            .count(),
            1
        );
    }

    #[test]
    fn rooted_plan_without_dependencies_still_compiles_a_task() {
        use arsy_kernel::{
            domain::SessionId,
            event::{EventStore, MemoryEventStore},
        };

        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let root = TaskId::new();
        let mut graph = TaskGraph::new(Arc::clone(&store), session, Principal::System).unwrap();
        graph
            .add(TaskNode {
                id: root,
                goal: "planning".into(),
                dependencies: Vec::new(),
                assignee: None,
                required_output: "text".into(),
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget: Budget {
                    tokens: 500,
                    cost_micros: 1_000,
                    wall_ms: 2_000,
                },
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: TaskRuntime::default(),
            })
            .unwrap();
        let journal = Journal {
            store: Arc::clone(&store),
            session,
            actor: Principal::System,
            task: Some(root),
            attempt: None,
        };
        let executors = PlanExecutor::executors(
            &state_for(dir.path(), "independent-plan"),
            &artifacts,
            0,
            Some(&journal),
        );
        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "single change"}),
        );
        let committed = commit_for(dir.path(), "independent-plan", &journal).unwrap();
        let id = committed.steps[0].committed_task.unwrap();
        let restored = TaskGraph::new(store, session, Principal::System).unwrap();
        let node = restored.node(id).unwrap();
        assert!(node.dependencies.is_empty());
        assert_eq!(node.runtime.plan.as_ref().unwrap().root, Some(root));
        assert_eq!(node.budget.tokens, 500);
        assert_eq!(node.state, TaskState::Pending);
    }

    #[test]
    fn missing_planning_root_does_not_commit_orphan_todos() {
        use arsy_kernel::{
            domain::SessionId,
            event::{EventStore, MemoryEventStore},
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
            task: Some(TaskId::new()),
            attempt: None,
        };
        let executors = PlanExecutor::executors(
            &state_for(dir.path(), "bad-root"),
            &artifacts,
            0,
            Some(&journal),
        );
        call(
            &executors,
            &artifacts,
            "plan.add",
            serde_json::json!({"description": "needs a planning task"}),
        );
        assert!(commit_for(dir.path(), "bad-root", &journal).is_err());
        assert!(TodoList::open(store, session, Principal::System)
            .unwrap()
            .snapshot()
            .items
            .is_empty());
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
