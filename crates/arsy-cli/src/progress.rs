//! Where the work stands: the turn's plan, and the session's checklist.
//!
//! One projection, two readers. The TUI draws it on `/plan` and `/todo`; a
//! scripted `arsy run` puts the same object in its result record. They share
//! this module rather than each formatting the state themselves, because a
//! terminal that disagreed with the JSON about which step is current would be
//! worse than either surface not existing.
//!
//! Nothing here dispatches an operation. A view of the harness's own state is
//! not a tool call: going through `plan.list` to draw a sidebar would need a
//! capability decision and an artifact write to answer a question the process
//! can already answer.

use arsy_code::agent::planops::{PlanSnapshot, PlanStepStatus};
use arsy_kernel::{
    domain::{Principal, SessionId},
    event::EventStore,
    todo::{TodoList, TodoSnapshot},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

/// The plan one scope holds right now, as a machine-readable projection.
pub fn plan(workspace: &Path, scope: &str) -> Value {
    projected_plan(&arsy_code::agent::planops::snapshot_for(workspace, scope))
}

fn projected_plan(snapshot: &PlanSnapshot) -> Value {
    json!({
        "steps": snapshot
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| json!({
                "position": index + 1,
                "id": step.id,
                "description": step.description,
                "status": match step.status {
                    PlanStepStatus::Pending => "pending",
                    PlanStepStatus::InProgress => "in_progress",
                    PlanStepStatus::Completed => "completed",
                },
                "current": snapshot.current().is_some_and(|current| current.id == step.id),
            }))
            .collect::<Vec<_>>(),
        "total": snapshot.steps.len(),
        "completed": snapshot.completed(),
        "current": snapshot.current().map(|step| step.id.clone()),
    })
}

/// The durable checklist a session holds, or `None` when it has none.
///
/// Read straight from the stream rather than from a cached runtime, so a view
/// opened in one process shows what another process wrote.
pub fn todos(store: Arc<dyn EventStore>, session: SessionId) -> Option<Value> {
    let list = TodoList::open(store, session, Principal::System).ok()?;
    Some(projected_todos(&list.snapshot()))
}

fn projected_todos(snapshot: &TodoSnapshot) -> Value {
    json!({
        "items": snapshot
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| json!({
                "position": index + 1,
                "id": item.id,
                "text": item.text,
                "status": item.status.as_str(),
                "author": item.author,
                "depends_on": item.depends_on,
                "blocked_by": item.blocked_by(&snapshot.items),
                "current": snapshot.current.as_deref() == Some(item.id.as_str()),
            }))
            .collect::<Vec<_>>(),
        "total": snapshot.total,
        "completed": snapshot.completed,
        "current": snapshot.current,
    })
}

/// A marker wide enough to be scanned down a column, and narrow enough to
/// leave the text room on a small terminal.
fn marker(status: &str) -> &'static str {
    match status {
        "completed" => "[x]",
        "in_progress" => "[~]",
        "cancelled" => "[-]",
        _ => "[ ]",
    }
}

pub fn human_plan(projection: &Value) -> String {
    let steps = projection["steps"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    if steps.is_empty() {
        return "No plan has been made for this task yet.".to_owned();
    }
    let mut text = format!(
        "{} of {} step(s) done\n",
        projection["completed"].as_u64().unwrap_or(0),
        projection["total"].as_u64().unwrap_or(0)
    );
    for step in steps {
        text.push_str(&row(step, "description"));
    }
    text
}

pub fn human_todos(projection: &Value) -> String {
    let items = projection["items"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    if items.is_empty() {
        return "This session has no TODOs.".to_owned();
    }
    let mut text = format!(
        "{} of {} TODO(s) done\n",
        projection["completed"].as_u64().unwrap_or(0),
        projection["total"].as_u64().unwrap_or(0)
    );
    for item in items {
        text.push_str(&row(item, "text"));
        let blocked = item["blocked_by"].as_array().map_or(&[][..], Vec::as_slice);
        if !blocked.is_empty() {
            text.push_str(&format!(
                "        waiting for {}\n",
                blocked
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    text
}

/// One line of either listing. `>` marks where the work is, so a long list
/// still answers "what now" without counting.
fn row(entry: &Value, body: &str) -> String {
    format!(
        "  {} {} {}\n",
        if entry["current"] == Value::Bool(true) {
            ">"
        } else {
            " "
        },
        marker(entry["status"].as_str().unwrap_or("pending")),
        entry[body].as_str().unwrap_or("?")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        event::MemoryEventStore,
        todo::{TodoAuthor, TodoStatus},
    };

    /// The two surfaces read one projection, so the terminal cannot disagree
    /// with the JSON about which step is current.
    #[test]
    fn a_plan_projection_marks_progress_and_the_step_being_worked() {
        let directory = tempfile::tempdir().unwrap();
        let scope = "projection";
        let state = arsy_code::agent::planops::state_for(directory.path(), scope);
        // Written through the state the operations share, which is what a turn
        // leaves behind after calling `plan_add`.
        {
            let mut plan = state.lock().unwrap();
            *plan = arsy_code::agent::planops::PlanState::default();
        }
        let empty = plan(directory.path(), scope);
        assert_eq!(empty["total"], 0);
        assert!(human_plan(&empty).contains("No plan"));

        let snapshot = PlanSnapshot {
            steps: vec![
                arsy_code::agent::planops::PlanStep {
                    id: "step-1".into(),
                    description: "read the test".into(),
                    status: PlanStepStatus::Completed,
                    committed_as: None,
                },
                arsy_code::agent::planops::PlanStep {
                    id: "step-2".into(),
                    description: "fix the bug".into(),
                    status: PlanStepStatus::InProgress,
                    committed_as: None,
                },
                arsy_code::agent::planops::PlanStep {
                    id: "step-3".into(),
                    description: "run the suite".into(),
                    status: PlanStepStatus::Pending,
                    committed_as: None,
                },
            ],
        };
        let projection = projected_plan(&snapshot);
        assert_eq!(projection["completed"], 1);
        assert_eq!(projection["current"], "step-2");
        assert_eq!(projection["steps"][1]["current"], true);
        assert_eq!(projection["steps"][0]["position"], 1);

        let text = human_plan(&projection);
        assert!(text.contains("1 of 3"), "{text}");
        assert!(text.contains("  > [~] fix the bug"), "{text}");
        assert!(text.contains("    [x] read the test"), "{text}");
        assert!(text.contains("    [ ] run the suite"), "{text}");
    }

    #[test]
    fn a_todo_projection_names_what_each_item_is_waiting_for() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        {
            let mut list = TodoList::open(Arc::clone(&store), session, Principal::System).unwrap();
            let first = list
                .add("land the fix", Vec::new(), TodoAuthor::User)
                .unwrap();
            list.add(
                "write the changelog",
                vec![first.id.clone()],
                TodoAuthor::Model,
            )
            .unwrap();
            list.update(&first.id, Some(TodoStatus::InProgress), None)
                .unwrap();
        }

        let projection = todos(store, session).expect("the session has a checklist");
        assert_eq!(projection["total"], 2);
        assert_eq!(projection["completed"], 0);
        assert_eq!(projection["items"][0]["current"], true);
        assert_eq!(projection["items"][1]["blocked_by"][0], "todo-1");

        let text = human_todos(&projection);
        assert!(text.contains("  > [~] land the fix"), "{text}");
        assert!(text.contains("waiting for todo-1"), "{text}");
    }
}
