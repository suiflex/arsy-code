//! `arsy session`: read the recorded event log, and branch it without ever
//! rewriting it.
//!
//! Every subcommand but `rewind` and `fork` is read-only. `rewind` and `fork`
//! append one `session.branched` event to a *new* stream; the parent keeps
//! every event it had, which is what makes an audit trail worth keeping.

use crate::{storage_failed, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_kernel::{
    artifact::unix_time_ms,
    domain::{EventId, SessionId},
    event::{EventPayload, EventStore},
    hub::{AgentHub, HubFilter},
    orchestration::TaskGraph,
    projection::{ProjectionSet, TurnStatus},
    service::{AgentService, BranchMode, ServiceError},
    sqlite::SessionSummary,
};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

/// How many sessions `session list` reports when the caller does not say.
const DEFAULT_LIMIT: usize = 50;

pub fn parse_migrate(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    if !arguments.positional.is_empty() {
        return Err(usage("migrate takes no positional argument"));
    }
    Ok(Command::Migrate {
        apply: arguments.apply,
        backup: arguments.backup.clone(),
    })
}

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.is_empty() {
        return Err(usage(HELP));
    }
    let action = positional.remove(0);
    let identifier = |positional: Vec<String>| -> Result<SessionId, Diagnostic> {
        let id = crate::only_argument(positional, &format!("session {action}"), "<SESSION_ID>")?;
        id.parse()
            .map_err(|_| usage(format!("`{id}` is not a canonical session ID")))
    };
    match action.as_str() {
        "list" => {
            if !positional.is_empty() {
                return Err(usage("session list takes no positional argument"));
            }
            Ok(Command::SessionList {
                limit: arguments.limit.unwrap_or(DEFAULT_LIMIT),
            })
        }
        "show" => Ok(Command::SessionShow {
            session: identifier(positional)?,
            turns: arguments.turns,
            evidence: arguments.evidence,
        }),
        "export" => {
            if arguments.include_artifacts && arguments.out.is_none() {
                return Err(usage(
                    "session export --include-artifacts needs --out: artifacts are written beside the export",
                ));
            }
            Ok(Command::SessionExport {
                session: identifier(positional)?,
                out: arguments.out.clone(),
                include_artifacts: arguments.include_artifacts,
            })
        }
        "rewind" => Ok(Command::SessionBranch {
            session: identifier(positional)?,
            at: Some(event_id(arguments.to.as_deref().ok_or_else(|| {
                usage("session rewind requires --to <EVENT_ID>")
            })?)?),
            mode: BranchMode::Rewind,
        }),
        "fork" => Ok(Command::SessionBranch {
            session: identifier(positional)?,
            at: arguments.at.as_deref().map(event_id).transpose()?,
            mode: BranchMode::Fork,
        }),
        "delete" | "remove" | "rm" => Ok(Command::SessionDelete {
            session: identifier(positional)?,
        }),
        "rename" => {
            if positional.is_empty() {
                return Err(usage("session rename requires <SESSION_ID> <TITLE>"));
            }
            let session_raw = positional.remove(0);
            let session = session_raw
                .parse()
                .map_err(|_| usage(format!("`{session_raw}` is not a canonical session ID")))?;
            let title = positional.join(" ");
            if title.trim().is_empty() {
                return Err(usage("session rename requires a non-empty <TITLE>"));
            }
            Ok(Command::SessionRename { session, title })
        }
        other => Err(usage(format!(
            "unknown session subcommand `{other}`\n{HELP}"
        ))),
    }
}

const HELP: &str = "session requires `list`, `show <ID>`, `export <ID>`, `delete <ID>`, `rename <ID> <TITLE>`, `rewind <ID> --to <EVENT_ID>`, or `fork <ID> [--at <EVENT_ID>]`";
fn event_id(value: &str) -> Result<EventId, Diagnostic> {
    value
        .parse()
        .map_err(|_| usage(format!("`{value}` is not a canonical event ID")))
}

/// `arsy session list`: one row per recorded stream, most recent first.
///
/// The store is per-workspace, so every session listed belongs to this
/// workspace; there is no cross-workspace index to filter against.
pub fn list(
    invocation: &Invocation,
    limit: usize,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = crate::open_store(&root)?;
    let summaries = store.sessions(limit).map_err(storage_failed)?;
    let mut rows = Vec::with_capacity(summaries.len());
    for summary in &summaries {
        rows.push(row(store.as_ref(), summary)?);
    }
    let report = json!({
        "workspace": root.display().to_string(),
        "limit": limit,
        "sessions": rows,
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        human_list(&report)
    });
    Ok(0)
}

/// `arsy session delete <ID>`: remove a recorded session stream and its events.
pub fn delete(
    invocation: &Invocation,
    session: SessionId,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = crate::open_store(&root)?;
    let deleted = store.delete_session(session).map_err(storage_failed)?;
    let report = json!({
        "session": session.to_string(),
        "deleted": deleted,
        "message": if deleted {
            format!("Deleted session {session}.")
        } else {
            format!("Session {session} was not found.")
        },
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"status": if deleted { "deleted" } else { "not_found" }})
    });
    Ok(0)
}

/// `arsy session rename <ID> <TITLE>`: assign a title to a recorded session.
pub fn rename(
    invocation: &Invocation,
    session: SessionId,
    title: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = crate::open_store(&root)?;
    store
        .set_session_title(session, title)
        .map_err(storage_failed)?;
    let report = json!({
        "session": session.to_string(),
        "title": title,
        "message": format!("Renamed session {session} to \"{title}\"."),
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"status": "renamed", "title": title})
    });
    Ok(0)
}

fn row(store: &dyn EventStore, summary: &SessionSummary) -> Result<Value, Diagnostic> {
    let history = AgentService::history(store, summary.session).map_err(storage_failed)?;
    let projection = ProjectionSet::rebuild(summary.session, &history).map_err(storage_failed)?;
    let usage = projection.usage();
    let ancestry = AgentService::ancestry(store, summary.session).map_err(storage_failed)?;
    Ok(json!({
        "session": summary.session.to_string(),
        "title": summary.title.clone(),
        "events": summary.version.0,
        "durability": format!("{:?}", summary.durability).to_lowercase(),
        "started_at_ms": summary.started_at_ms,
        "last_event_at_ms": summary.last_event_at_ms,
        "status": status(&projection),
        "turns": projection.turns().len(),
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cost_micros": usage.cost_micros,
        "branched_from": ancestry.as_ref().map(|evidence| json!({
            "session": evidence.parent.to_string(),
            "mode": evidence.mode.as_str(),
            "at_sequence": evidence.at_sequence,
        })),
    }))
}

/// A cost total as an operator reads it.
///
/// `null` is "unknown", which is what an unpriced model produces. Printed as
/// the word rather than as `$0.00`, because a running total that silently
/// reported unpriced spend as free would be the one thing this whole path
/// exists to avoid.
fn money(micros: &Value) -> String {
    match micros.as_u64() {
        Some(micros) => format!("${:.4}", micros as f64 / 1_000_000.0),
        None => "unknown (no pricing configured)".to_owned(),
    }
}

/// A session is as unfinished as its least finished turn: one running turn
/// makes the whole session running, however many completed before it.
fn status(projection: &ProjectionSet) -> &'static str {
    let mut failed = false;
    for turn in projection.turns().values() {
        match turn.status {
            TurnStatus::Running => return "running",
            TurnStatus::Failed => failed = true,
            TurnStatus::Completed => {}
        }
    }
    match (failed, projection.turns().is_empty()) {
        (true, _) => "failed",
        (false, true) => "empty",
        (false, false) => "completed",
    }
}

fn human_list(report: &Value) -> Value {
    let sessions = report["sessions"].as_array().map_or(&[][..], Vec::as_slice);
    if sessions.is_empty() {
        return json!({
            "sessions": format!(
                "No sessions are recorded in {}.",
                report["workspace"].as_str().unwrap_or(".")
            ),
        });
    }
    let mut listing = format!(
        "{} session{} in {}\n",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" },
        report["workspace"].as_str().unwrap_or(".")
    );
    for session in sessions {
        // What the session is called leads; the full id follows it, because
        // that is what `arsy resume` and `arsy session show` take.
        let id = session["session"].as_str().unwrap_or("?");
        let name = match session["title"].as_str() {
            Some(title) => format!("\"{title}\" · {id}"),
            None => id.to_owned(),
        };
        let mut row = format!(
            "\n  {} · {} · {} turn(s) · {} event(s)\n    tokens: {} in / {} out · cost: {} · started {}",
            name,
            session["status"].as_str().unwrap_or("?"),
            session["turns"],
            session["events"],
            session["input_tokens"],
            session["output_tokens"],
            money(&session["cost_micros"]),
            timestamp(&session["started_at_ms"]),
        );
        row.push('\n');
        listing.push_str(&row);
        if let Some(parent) = session["branched_from"].as_object() {
            listing.push_str(&format!(
                "    branched from {} ({} at sequence {})\n",
                parent["session"].as_str().unwrap_or("?"),
                parent["mode"].as_str().unwrap_or("?"),
                parent["at_sequence"],
            ));
        }
    }
    json!({ "sessions": listing })
}

/// `arsy session show`: the projection a resumed session would rebuild.
pub fn show(
    invocation: &Invocation,
    session: SessionId,
    turns: bool,
    evidence: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let store = crate::open_store(&crate::workspace_root(&invocation.workspace)?)?;
    let history = require_session(store.as_ref(), session)?;
    let projection = ProjectionSet::rebuild(session, &history).map_err(storage_failed)?;
    let usage = projection.usage();
    let ancestry = AgentService::ancestry(store.as_ref(), session).map_err(storage_failed)?;
    let graph = TaskGraph::new(store.clone(), session, crate::actor()).map_err(storage_failed)?;
    let hub = AgentHub::project(&graph, unix_time_ms(), HubFilter::All);

    let mut report = json!({
        "session": session.to_string(),
        "events": projection.applied_version().0,
        "status": status(&projection),
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cost_micros": usage.cost_micros,
        },
        "turns": projection.turns().len(),
        "agents": hub.rows,
        "branched_from": ancestry.as_ref().map(|branch| json!({
            "session": branch.parent.to_string(),
            "mode": branch.mode.as_str(),
            "at_event": branch.at_event.to_string(),
            "at_sequence": branch.at_sequence,
            "parent_version": branch.parent_version,
            "inherits_prefix": branch.mode.inherits_prefix(),
        })),
    });
    if turns {
        report["turn_detail"] = Value::Array(
            projection
                .turns()
                .values()
                .map(|turn| {
                    json!({
                        "turn": turn.id.to_string(),
                        "status": format!("{:?}", turn.status).to_lowercase(),
                        "started_sequence": turn.started_sequence,
                        "finished_sequence": turn.finished_sequence,
                    })
                })
                .collect(),
        );
    }
    if evidence {
        report["timeline"] = Value::Array(
            history
                .iter()
                .map(|event| {
                    json!({
                        "sequence": event.sequence,
                        "event": event.id.to_string(),
                        "kind": event.kind,
                        "actor": event.actor,
                        "occurred_at_ms": event.occurred_at_ms,
                        "payload": match &event.payload {
                            EventPayload::Inline { data } => data.clone(),
                            EventPayload::Artifact { reference, media_type, size } => json!({
                                "artifact": reference.value(),
                                "media_type": media_type,
                                "size": size,
                            }),
                        },
                    })
                })
                .collect(),
        );
    }
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        human_show(&report)
    });
    Ok(0)
}

fn human_show(report: &Value) -> Value {
    let mut text = format!(
        "session {}\n  status: {} · {} turn(s) · {} event(s)\n  tokens: {} in / {} out · cost: {}\n",
        report["session"].as_str().unwrap_or("?"),
        report["status"].as_str().unwrap_or("?"),
        report["turns"],
        report["events"],
        report["usage"]["input_tokens"],
        report["usage"]["output_tokens"],
        money(&report["usage"]["cost_micros"]),
    );
    if let Some(branch) = report["branched_from"].as_object() {
        text.push_str(&format!(
            "  branched from {} ({}) at sequence {}; inherits prefix: {}\n",
            branch["session"].as_str().unwrap_or("?"),
            branch["mode"].as_str().unwrap_or("?"),
            branch["at_sequence"],
            branch["inherits_prefix"],
        ));
    }
    if let Some(rows) = report["agents"].as_array().filter(|rows| !rows.is_empty()) {
        text.push_str(&format!("  agents: {}\n", rows.len()));
        for row in rows {
            text.push_str(&format!(
                "    {} · {} · {} · proof {}\n",
                row["task"].as_str().unwrap_or("?"),
                row["role"]
                    .as_str()
                    .filter(|role| !role.is_empty())
                    .unwrap_or("unassigned"),
                row["task_state"].as_str().unwrap_or("?"),
                row["proof"].as_str().unwrap_or("pending"),
            ));
        }
    }
    for turn in report["turn_detail"].as_array().unwrap_or(&Vec::new()) {
        text.push_str(&format!(
            "  turn {} · {} · sequences {}..{}\n",
            turn["turn"].as_str().unwrap_or("?"),
            turn["status"].as_str().unwrap_or("?"),
            turn["started_sequence"],
            turn["finished_sequence"],
        ));
    }
    for entry in report["timeline"].as_array().unwrap_or(&Vec::new()) {
        text.push_str(&format!(
            "  #{} {} at {}\n",
            entry["sequence"],
            entry["kind"].as_str().unwrap_or("?"),
            timestamp(&entry["occurred_at_ms"]),
        ));
    }
    json!({ "session": text })
}

/// `arsy session export`: the canonical events, one JSON object per line.
///
/// JSONL rather than one array so an export of a long session streams and a
/// truncated file still parses up to its last complete line.
pub fn export(
    invocation: &Invocation,
    session: SessionId,
    out: Option<&Path>,
    include_artifacts: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = crate::open_store(&root)?;
    let history = require_session(store.as_ref(), session)?;

    let mut exported = 0_u64;
    match out {
        None => {
            let stdout = std::io::stdout();
            let mut sink = BufWriter::new(stdout.lock());
            for event in &history {
                write_event(&mut sink, event, &mut exported)?;
            }
            sink.flush().map_err(storage_failed)?;
        }
        Some(path) => {
            let file = File::create(path)
                .map_err(|error| usage(format!("cannot write {}: {error}", path.display())))?;
            let mut sink = BufWriter::new(file);
            for event in &history {
                write_event(&mut sink, event, &mut exported)?;
            }
            sink.flush().map_err(storage_failed)?;
        }
    }

    let artifacts = if include_artifacts {
        let directory = out
            .expect("parsing rejects --include-artifacts without --out")
            .with_extension("artifacts");
        crate::evidence::export_referenced(&root, &history, &directory)?
    } else {
        Value::Null
    };

    emitter.result(json!({
        "session": session.to_string(),
        "events": exported,
        "out": out.map(|path| path.display().to_string()),
        "artifacts": artifacts,
    }));
    Ok(0)
}

fn write_event(
    sink: &mut impl Write,
    event: &arsy_kernel::event::EventEnvelope,
    exported: &mut u64,
) -> Result<(), Diagnostic> {
    let line = serde_json::to_string(event).map_err(storage_failed)?;
    writeln!(sink, "{line}").map_err(storage_failed)?;
    *exported = exported.saturating_add(1);
    Ok(())
}

/// `arsy session rewind` and `arsy session fork`: open a branch.
pub fn branch(
    invocation: &Invocation,
    session: SessionId,
    at: Option<EventId>,
    mode: BranchMode,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let store = crate::open_store(&crate::workspace_root(&invocation.workspace)?)?;
    require_session(store.as_ref(), session)?;
    let (_, branch, evidence) = AgentService::branch(store, session, at, mode, crate::actor())
        .map_err(|error| match error {
            // A branch point the caller named is their input, not a storage
            // fault: it must exit as a usage error so a script can tell the
            // two apart.
            ServiceError::UnknownEvent(_) | ServiceError::EmptyStream(_) => {
                usage(error.to_string())
            }
            other => storage_failed(other),
        })?;
    emitter.session = Some(branch);
    emitter.result(json!({
        "session": branch.to_string(),
        "mode": mode.as_str(),
        "parent": evidence.parent.to_string(),
        "at_event": evidence.at_event.to_string(),
        "at_sequence": evidence.at_sequence,
        "parent_version": evidence.parent_version,
        "inherits_prefix": mode.inherits_prefix(),
        "parent_truncated": false,
    }));
    Ok(0)
}

/// Read a session, refusing one this workspace has never recorded.
///
/// A silent empty result would read as "the session finished with nothing in
/// it", which is a different fact from "that ID is not here".
fn require_session(
    store: &dyn EventStore,
    session: SessionId,
) -> Result<Vec<arsy_kernel::event::EventEnvelope>, Diagnostic> {
    let history = AgentService::history(store, session).map_err(storage_failed)?;
    if history.is_empty() {
        return Err(Diagnostic::error(
            "ARSY-SCH-1004",
            format!("session {session} has no recorded events in this workspace"),
            "list what is recorded with `arsy session list`",
        ));
    }
    Ok(history)
}

/// Milliseconds since the epoch as a readable UTC stamp.
///
/// The record keeps the integer; a person reading a listing needs a date, and
/// pulling in a date library for one format string is not worth the dependency.
pub fn timestamp(value: &Value) -> String {
    let Some(ms) = value.as_u64() else {
        return "unknown".to_owned();
    };
    let seconds = (ms / 1000) as i64;
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Howard Hinnant's `civil_from_days`, the standard shift-to-March algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `arsy migrate`: report the session store's schema migration, then apply it.
///
/// Planning never writes, so the default run is a report and `--apply` is the
/// only path that touches the store. The engine takes a verified backup before
/// the first step runs; this command only decides where it goes.
pub fn migrate(
    invocation: &Invocation,
    apply: bool,
    backup: Option<&Path>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = root.join(crate::STORE_PATH);
    if !store.exists() {
        return Err(Diagnostic::error(
            "ARSY-CMP-1001",
            format!("there is no session store at {}", store.display()),
            "run a task in this workspace first: the store is created with the first session",
        ));
    }

    let plan = arsy_kernel::migrate::plan(&store).map_err(storage_failed)?;
    let mut report = json!({
        "store": store.display().to_string(),
        "current": plan.current,
        "target": plan.target,
        "current_schema": plan.is_current(),
        "applied": false,
        "steps": plan
            .steps
            .iter()
            .map(|step| json!({
                "from": step.from,
                "to": step.to,
                "description": step.description,
                "loss": step.loss,
            }))
            .collect::<Vec<_>>(),
        "loss": plan.loss,
    });

    if apply && !plan.is_current() {
        let backup = backup.map_or_else(
            || store.with_file_name(format!("sessions.v{}.backup", plan.current)),
            Path::to_path_buf,
        );
        arsy_kernel::migrate::apply(&store, &backup).map_err(storage_failed)?;
        report["applied"] = json!(true);
        report["backup"] = json!(backup.display().to_string());
    }

    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({ "migrate": human_migration(&report, &plan) })
    });
    Ok(0)
}

fn human_migration(report: &Value, plan: &arsy_kernel::migrate::MigrationPlan) -> String {
    if plan.is_current() {
        return format!("Schema version {} is current.", plan.current);
    }
    let mut text = format!(
        "{} step(s) from schema version {} to {}\n",
        plan.steps.len(),
        plan.current,
        plan.target
    );
    for step in &plan.steps {
        text.push_str(&format!(
            "  {} → {} · {}\n",
            step.from, step.to, step.description
        ));
    }
    for loss in &plan.loss {
        text.push_str(&format!("  loses: {loss}\n"));
    }
    match report.get("backup").and_then(Value::as_str) {
        Some(backup) => text.push_str(&format!("Applied. The original is at {backup}.")),
        None => text.push_str("Nothing was written. Re-run with --apply."),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_render_as_utc_dates() {
        assert_eq!(timestamp(&json!(0_u64)), "1970-01-01 00:00:00Z");
        assert_eq!(
            timestamp(&json!(1_700_000_000_000_u64)),
            "2023-11-14 22:13:20Z"
        );
        assert_eq!(timestamp(&json!(null)), "unknown");
    }

    #[test]
    fn session_subcommands_validate_their_arguments() {
        let id = SessionId::new().to_string();
        for args in [
            vec!["session".to_owned(), "list".to_owned()],
            vec!["session".to_owned(), "show".to_owned(), id.clone()],
            vec!["session".to_owned(), "export".to_owned(), id.clone()],
            vec![
                "session".to_owned(),
                "rewind".to_owned(),
                id.clone(),
                "--to".to_owned(),
                EventId::new().to_string(),
            ],
            vec!["session".to_owned(), "fork".to_owned(), id.clone()],
        ] {
            assert!(crate::parse(args.clone()).is_ok(), "{args:?}");
        }
        for args in [
            vec!["session".to_owned()],
            vec!["session".to_owned(), "list".to_owned(), id.clone()],
            vec![
                "session".to_owned(),
                "show".to_owned(),
                "not-a-uuid".to_owned(),
            ],
            // A rewind without a point would silently mean "the head", which is
            // not a rewind at all.
            vec!["session".to_owned(), "rewind".to_owned(), id.clone()],
            vec![
                "session".to_owned(),
                "export".to_owned(),
                id,
                "--include-artifacts".to_owned(),
            ],
        ] {
            assert!(crate::parse(args.clone()).is_err(), "{args:?}");
        }
    }

    #[test]
    fn a_listing_shows_the_title_a_session_was_given() {
        let row = |session: &str, title: Value| {
            json!({
                "session": session,
                "title": title,
                "status": "completed",
                "turns": 2,
                "events": 9,
                "input_tokens": 10,
                "output_tokens": 4,
                "cost_micros": 1_000,
                "started_at_ms": 0,
            })
        };
        let report = json!({
            "workspace": "/w",
            "sessions": [
                row("11111111-1111-1111-1111-111111111111", json!("feature work")),
                row("22222222-2222-2222-2222-222222222222", Value::Null),
            ],
        });
        let listing = human_list(&report)["sessions"].as_str().unwrap().to_owned();

        assert!(
            listing.contains("\"feature work\" · 11111111-1111-1111-1111-111111111111"),
            "the title leads and the id follows: {listing}"
        );
        assert!(
            listing.contains("\n  22222222-2222-2222-2222-222222222222 · "),
            "an untitled session is its id alone: {listing}"
        );
    }
}
