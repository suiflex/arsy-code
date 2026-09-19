//! `arsy serve --protocol acp`: the editor's side of a session, on stdio.
//!
//! # What this loop is, and is not
//!
//! It is the transport and the session lifecycle: read a JSON-RPC line, hand
//! the method to [`AcpAdapter`], and act on what comes back. It decides
//! nothing else. A prompt becomes a task in the same durable graph `arsy run`
//! uses, executed by the same [`crate::TaskRun`], under the same policy — so
//! an editor holds no authority a shell does not, and a turn started from an
//! editor is resumable by `arsy resume` like any other.
//!
//! # Streaming
//!
//! Model text reaches the editor as `session/update` notifications while the
//! turn runs, which is why the emitter has an ACP mode rather than the loop
//! collecting a reply and sending it at the end. The response to
//! `session/prompt` carries only the stop reason, as ACP specifies.
//!
//! # Cancellation
//!
//! One connection, one thread, and a turn that holds it: a `session/cancel`
//! that arrives mid-turn is not read until the turn returns. So cancelling
//! cancels the session's *unfinished* tasks, which is real and bounded, and
//! the response says how many. Interrupting a turn in flight needs a transport
//! that can read while one is running; this build does not claim to.

use crate::{storage_failed, Diagnostic, Emitter, Invocation, Output, TaskRun};
use arsy_code::acp::{AcpAdapter, AcpError, Translated};
use arsy_kernel::{
    domain::{SessionId, WorkspaceId},
    event::EventStore,
    orchestration::TaskGraph,
    protocol::ClientRequest,
};
use serde_json::{json, Value};
use std::io::{BufReader, Write};

pub fn run(invocation: &Invocation, _emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let adapter = AcpAdapter::new(&root, WorkspaceId::new());

    // The protocol owns stdout: nothing here writes ARSY's own record framing,
    // so a client never has to filter it out of the stream.
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        let Some(line) = crate::serve::read_line(&mut reader)? else {
            return Ok(0);
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line.trim()) {
            Ok(message) => message,
            Err(error) => {
                // No id to correlate against, so the error is uncorrelated
                // rather than attributed to a request the client never made.
                respond(&json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": error.to_string()},
                }))?;
                continue;
            }
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = message.get("params").cloned().unwrap_or(json!({}));

        let answer = match adapter.request(&method, &params) {
            Err(error) => Err(failure(&error)),
            Ok(Translated::Answer(value)) => Ok(value),
            // The adapter validates the request; the session it names comes
            // from the parameters, because ACP addresses a cancellation by
            // session and the canonical control message addresses an agent.
            Ok(Translated::Request(ClientRequest::AgentControl(_))) => session_of(&params)
                .map_err(|error| failure(&error))
                .and_then(|session| {
                    cancel(invocation, session)
                        .map(|cancelled| json!({"cancelled": cancelled}))
                        .map_err(|error| failed(&error))
                }),
            Ok(Translated::Request(request)) => handle(invocation, &adapter, request),
        };
        // A notification -- no id -- is answered by doing the work and saying
        // nothing, which is what JSON-RPC requires.
        if id.is_null() {
            continue;
        }
        respond(&match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        })?;
    }
}

fn handle(
    invocation: &Invocation,
    adapter: &AcpAdapter,
    request: ClientRequest,
) -> Result<Value, Value> {
    match request {
        // A session is a stream id and nothing else until it is written to, so
        // creating one costs nothing and needs no provider.
        ClientRequest::SessionCreate(_) => Ok(json!({"sessionId": SessionId::new().to_string()})),
        ClientRequest::SessionResume(resume) => {
            let events = recorded(invocation, resume.session).map_err(|error| failed(&error))?;
            if events == 0 {
                return Err(failed(&Diagnostic::error(
                    "ARSY-SCH-1004",
                    format!("session {} has no recorded events here", resume.session),
                    "open a session this workspace recorded, or start a new one",
                )));
            }
            Ok(json!({"sessionId": resume.session.to_string(), "events": events}))
        }
        ClientRequest::TurnStart(turn) => prompt(invocation, turn.session, &turn.prompt),
        other => {
            let _ = adapter;
            Err(failure(&AcpError::UnknownMethod(format!("{other:?}"))))
        }
    }
}

/// Run one prompt as a task, streaming what the model says as it says it.
///
/// The redactor is installed before the turn starts, not after. `dispatch`
/// streams every chunk through the emitter as it arrives, so an emitter whose
/// redactor is still empty delivers a credential the model echoed straight to
/// the editor -- and the same turn from a terminal would have masked it,
/// because `arsy run` installs the redactor while preparing the task.
fn prompt(invocation: &Invocation, session: SessionId, prompt: &str) -> Result<Value, Value> {
    let mut emitter = Emitter::new(Output::Acp);
    emitter.session = Some(session);
    // Also sanitizes the prompt, for the same reason `arsy run` does: an
    // editor may paste a key into a message, and it should not reach the
    // provider or the transcript verbatim.
    let prompt = crate::prepare_task(prompt, &mut emitter).map_err(|error| failed(&error))?;
    let mut execution = TaskRun::open(invocation, Some(session)).map_err(|error| failed(&error))?;
    let task = execution.enqueue(&prompt).map_err(|error| failed(&error))?;
    let code = execution
        .execute(task, Value::Null, &mut emitter)
        .map_err(|error| failed(&error))?;
    Ok(json!({
        "stopReason": if code == 0 { "end_turn" } else { "refusal" },
        "_meta": {"sessionId": session.to_string(), "taskId": task.to_string(), "exitCode": code},
    }))
}

/// Cancel a session's unfinished tasks. See the module note on what this does
/// not do.
fn cancel(invocation: &Invocation, session: SessionId) -> Result<usize, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = crate::open_store(&root)?;
    let mut graph = TaskGraph::new(store, session, crate::actor()).map_err(storage_failed)?;
    let pending: Vec<_> = graph.pending().iter().map(|node| node.id).collect();
    for task in &pending {
        graph
            .cancel(*task, "cancelled by the client")
            .map_err(storage_failed)?;
    }
    Ok(pending.len())
}

fn session_of(params: &Value) -> Result<SessionId, AcpError> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidParams("a sessionId is required".to_owned()))?
        .parse()
        .map_err(|_| AcpError::InvalidParams("sessionId is not a canonical ID".to_owned()))
}

fn recorded(invocation: &Invocation, session: SessionId) -> Result<u64, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    Ok(crate::open_store(&root)?
        .current_version(session)
        .map_err(|error| storage_failed(error.to_string()))?
        .0)
}

/// One `session/update` notification, as the emitter's ACP mode writes them.
pub fn notify(session: Option<SessionId>, update: Value) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session.map(|session| session.to_string()),
            "update": update,
        },
    });
    let _ = respond(&notification);
}

fn respond(message: &Value) -> Result<(), Diagnostic> {
    let encoded = serde_json::to_string(message).map_err(storage_failed)?;
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    writeln!(writer, "{encoded}")
        .and_then(|()| writer.flush())
        .map_err(storage_failed)
}

/// JSON-RPC error codes: an unknown method is the protocol's own -32601, and
/// everything else this adapter refuses is bad parameters.
fn failure(error: &AcpError) -> Value {
    let code = match error {
        AcpError::UnknownMethod(_) => -32601,
        AcpError::InvalidParams(_) | AcpError::OutsideWorkspace(_) => -32602,
    };
    json!({"code": code, "message": error.to_string()})
}

/// An ARSY diagnostic as a JSON-RPC error, keeping the code an operator would
/// see on the command line so the two surfaces can be correlated.
fn failed(diagnostic: &Diagnostic) -> Value {
    json!({
        "code": -32000,
        "message": diagnostic.message,
        "data": {"arsyCode": diagnostic.code, "remediation": diagnostic.remediation},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_errors_keep_their_json_rpc_codes() {
        assert_eq!(
            failure(&AcpError::UnknownMethod("session/dance".into()))["code"],
            -32601
        );
        assert_eq!(
            failure(&AcpError::InvalidParams("no sessionId".into()))["code"],
            -32602
        );
        let diagnostic = Diagnostic::error("ARSY-PRV-1000", "no credential", "run auth set");
        assert_eq!(failed(&diagnostic)["data"]["arsyCode"], "ARSY-PRV-1000");
    }
}
