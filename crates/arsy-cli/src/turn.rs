//! One interactive turn, whichever route runs it: the native provider loop
//! with its tool calls and approvals, the external Codex CLI projection, and
//! the recording of what the turn left behind.

use crate::run::{charge_turn, context_budget, is_stale_oauth_token, merge, prepare_task};
#[cfg(feature = "tui")]
use crate::*;
#[cfg(feature = "tui")]
use arsy_kernel::provider::Effort;
#[cfg(feature = "tui")]
use sha2::{Digest, Sha256};

#[cfg(feature = "tui")]
struct RecordedTurn {
    service: AgentService,
    graph: TaskGraph,
    actor: Principal,
    admission: arsy_kernel::service::TurnAdmission,
    session: SessionId,
    task: TaskId,
    store: Arc<dyn EventStore>,
}

#[cfg(feature = "tui")]
fn record_turn(
    invocation: &Invocation,
    session: SessionId,
    task: String,
    emitter: &mut Emitter,
) -> Result<RecordedTurn, Diagnostic> {
    let store: Arc<dyn EventStore> = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store), session).map_err(storage_failed)?;
    let mut graph =
        TaskGraph::new(Arc::clone(&store), session, actor.clone()).map_err(graph_failed)?;
    let agent = AgentId::new();
    let id = TaskId::new();
    graph
        .add(TaskNode {
            id,
            goal: task.clone(),
            dependencies: Vec::new(),
            assignee: Some(agent),
            required_output: "an answer to the task".to_owned(),
            workspace: WorkspaceRequirement::IsolatedWriter,
            budget: TASK_BUDGET,
            authority: Vec::new(),
            state: TaskState::Pending,
            lease_expires_at_ms: None,
            runtime: Default::default(),
        })
        .map_err(graph_failed)?;
    graph.ready().map_err(graph_failed)?;
    graph
        .lease(id, agent, unix_time_ms() + TASK_LEASE_MS)
        .map_err(graph_failed)?;

    let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: task,
        extensions: Extensions::new(),
    }));
    let admission = service
        .start_turn(actor.clone(), &envelope)
        .map_err(storage_failed)?;
    Ok(RecordedTurn {
        service,
        graph,
        actor,
        admission,
        session,
        task: id,
        store,
    })
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn persist_interrupted_turn(
    service: &AgentService,
    graph: &mut TaskGraph,
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    node: TaskId,
    route: &tui::ModelRoute,
    base: usize,
    conversation: &mut Vec<ModelMessage>,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    conversation.truncate(base);
    service
        .fail_turn(
            actor.clone(),
            turn_id,
            "user_interrupt",
            format!("{route} was interrupted"),
        )
        .map_err(storage_failed)?;
    graph
        .cancel(node, "interrupted by the operator")
        .map_err(graph_failed)?;
    turn_record(
        emitter,
        json!({
            "session": session.to_string(),
            "turn": turn_id.to_string(),
            "status": "interrupted",
        }),
    );
    Ok(())
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn persist_failed_turn(
    service: &AgentService,
    graph: &mut TaskGraph,
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    node: TaskId,
    failure: &str,
    base: usize,
    conversation: &mut Vec<ModelMessage>,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    conversation.truncate(base);
    graph
        .fail(node, json!({"message": failure}))
        .map_err(graph_failed)?;
    fail_turn(
        service,
        actor.clone(),
        turn_id,
        session,
        failure.to_owned(),
        emitter,
    )?;
    Ok(())
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn persist_completed_turn(
    service: &AgentService,
    graph: &mut TaskGraph,
    store: &Arc<dyn EventStore>,
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    session_id: SessionId,
    node: TaskId,
    route: &tui::ModelRoute,
    native: Option<&provider::Resolved>,
    colour: bool,
    base: usize,
    conversation: &mut Vec<ModelMessage>,
    turn: &Turn,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    if !turn.response.trim().is_empty() {
        conversation.push(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::Text {
                text: turn.response.clone(),
            }],
        });
    }
    let mut outcome = json!({"provider": route.provider, "model": route.model});
    merge(&mut outcome, turn.usage.clone());
    let priced = charge_turn(native, &route.model, &turn.usage);
    merge(
        &mut outcome,
        json!({
            "cost_micros": priced,
            "cost_source": if priced.is_some() { "configured" } else { "unknown" },
            "response": turn.response.clone(),
            "transcript": transcript::persistable(&conversation[base..]),
        }),
    );
    service
        .record_usage(
            actor.clone(),
            arsy_kernel::projection::UsageTotals {
                input_tokens: summary_number(&turn.usage, "input_tokens"),
                output_tokens: summary_number(&turn.usage, "output_tokens"),
                cost_micros: priced,
            },
        )
        .map_err(storage_failed)?;
    service
        .record_transcript(
            actor.clone(),
            turn_id,
            &transcript::persistable(&conversation[base..]),
        )
        .map_err(storage_failed)?;
    service
        .complete_turn(actor.clone(), turn_id, &outcome)
        .map_err(storage_failed)?;
    graph.complete(node, outcome).map_err(graph_failed)?;
    if !turn.response.trim().is_empty() {
        let events = store.current_version(session).map_err(storage_failed)?.0;
        let footer = tui::session_footer(
            &session_id.to_string(),
            turn.changed_files.len(),
            turn.rules_granted,
            events,
            colour,
        );
        let _ = writeln!(io::stdout(), "{}{footer}", modern_gap());
    }
    turn_record(
        emitter,
        json!({
            "session": session.to_string(),
            "turn": turn_id.to_string(),
            "status": "completed",
            "model": route.to_string(),
        }),
    );
    Ok(())
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn persist_turn(
    service: &AgentService,
    graph: &mut TaskGraph,
    store: &Arc<dyn EventStore>,
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    session_id: SessionId,
    node: TaskId,
    route: &tui::ModelRoute,
    native: Option<&provider::Resolved>,
    colour: bool,
    base: usize,
    conversation: &mut Vec<ModelMessage>,
    turn: &Turn,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    if turn.interrupted {
        return persist_interrupted_turn(
            service,
            graph,
            actor,
            turn_id,
            session,
            node,
            route,
            base,
            conversation,
            emitter,
        );
    }
    if let Some(failure) = &turn.failure {
        return persist_failed_turn(
            service,
            graph,
            actor,
            turn_id,
            session,
            node,
            failure,
            base,
            conversation,
            emitter,
        );
    }
    persist_completed_turn(
        service,
        graph,
        store,
        actor,
        turn_id,
        session,
        session_id,
        node,
        route,
        native,
        colour,
        base,
        conversation,
        turn,
        emitter,
    )
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_turn(
    invocation: &Invocation,
    session_id: SessionId,
    mut native: Option<&mut provider::Resolved>,
    task: &str,
    history: &arsy_code::agent::budget::History,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    colour: bool,
    footer: &str,
    conversation: &mut Vec<ModelMessage>,
    transcript: &mut tui::Transcript,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
    emitter: &mut Emitter,
) -> Result<Turn, Diagnostic> {
    let task = prepare_task(task, emitter)?;
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working, invocation.config.as_deref())?;
    // Loaded once per turn, as `arsy run` loads them: a turn finishes with the
    // hooks it began with.
    let loaded = hook_engine(&root, &config);
    let hooks = (!loaded.is_empty()).then_some(&loaded.engine);
    let task = turn_boundary(
        hooks,
        arsy_code::hook::LifecycleEvent::BeforeTurn,
        &task,
        emitter,
    )
    .map_err(|reason| {
        Diagnostic::error(
            "ARSY-HOK-1001",
            reason,
            "the hook that refused it is listed by `/hooks`",
        )
    })?;
    let RecordedTurn {
        service,
        mut graph,
        actor,
        admission,
        session,
        task: node,
        store,
    } = record_turn(invocation, session_id, task.clone(), emitter)?;
    // Where the conversation stood before this turn. A turn that fails or is
    // stopped rewinds to here, which is more than one message once the turn
    // has run tools.
    let base = conversation.len();
    conversation.push(ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text { text: task.clone() }],
    });
    emitter.trace(
        "turn.started",
        json!({
            "turn": admission.turn.to_string(),
            "provider": route.provider,
            "model": route.model,
            "restored_messages": base,
            "cited_events": history.citations.len(),
            "mode": approval.get().label(),
        }),
    );
    let outcome = match native.as_deref_mut() {
        Some(resolved) => {
            let runtime = agent_runtime(
                &root,
                &config,
                true,
                &session_id.to_string(),
                Some(session_id),
                None,
                Some(session_connector()),
                emitter,
                &prompt_skills(&root, &config),
            )?
            .with_execution_mode(approval.get().execution_mode());
            let outcome = native_turn(
                resolved,
                &config,
                &runtime,
                conversation,
                history,
                route,
                effort,
                admission.turn,
                colour,
                footer,
                keys,
                decoder,
                composer,
                transcript,
                approval,
                hooks,
            );
            if let Some(attempt) = graph
                .node(node)
                .and_then(|node| node.runtime.current_attempt)
            {
                for audit in runtime.take_safety_audits() {
                    graph
                        .record(
                            attempt,
                            "safety.review",
                            serde_json::to_value(audit).unwrap_or(Value::Null),
                        )
                        .map_err(graph_failed)?;
                }
            }
            outcome
        }
        None => external_status(
            &root,
            &task,
            route,
            approval,
            colour,
            footer,
            keys,
            decoder,
            composer,
            &emitter.redactor,
        ),
    };
    // A turn that never started leaves the composer painted, so it is torn down
    // here before the diagnostic is written over the input block.
    if outcome.is_err() {
        let mut stdout = io::stdout();
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
    }
    // A rule the operator granted is evidence of the turn they granted it in,
    // whether or not that turn then succeeded — so the cell is drained and
    // filed before the failure path returns, rather than being left for the
    // next turn to pick up and attribute to itself.
    let recorded_rules = approval.take_recorded();
    if !recorded_rules.is_empty() {
        service
            .record_approval(actor.clone(), admission.turn, &recorded_rules)
            .map_err(storage_failed)?;
    }
    let mut turn = match outcome {
        Ok(turn) => turn,
        Err(error) => {
            let reason = format!("could not run {route}: {error}");
            graph
                .fail(node, json!({"message": reason.clone()}))
                .map_err(graph_failed)?;
            fail_turn(&service, actor, admission.turn, session, reason, emitter)?;
            return Ok(Turn::default());
        }
    };
    turn.rules_granted = recorded_rules.len();
    if !turn.interrupted && turn.failure.is_none() {
        transcript.push_assistant(&turn.response);
    }
    // Whatever the turn ended as, which is what Codex's `notify` is for.
    // Nothing it returns can change what already happened.
    let stop = match (turn.interrupted, &turn.failure) {
        (true, _) => "interrupted",
        (false, Some(_)) => "failed",
        (false, None) => "answered",
    };
    let _ = turn_boundary(
        hooks,
        arsy_code::hook::LifecycleEvent::AfterTurn,
        stop,
        emitter,
    );
    emitter.trace(
        "turn.finished",
        json!({
            "turn": admission.turn.to_string(),
            "interrupted": turn.interrupted,
            "failed": turn.failure.is_some(),
            "response_bytes": turn.response.len(),
            "usage": turn.usage,
            "messages_added": conversation.len().saturating_sub(base),
        }),
    );
    persist_turn(
        &service,
        &mut graph,
        &store,
        &actor,
        admission.turn,
        session,
        session_id,
        node,
        route,
        native.as_deref(),
        colour,
        base,
        conversation,
        &turn,
        emitter,
    )?;
    Ok(turn)
}

/// What the operator said about one tool call.
#[cfg(feature = "tui")]
fn tool_call_fingerprint(name: &str, arguments: &Value) -> String {
    // A timeout is execution metadata, not command identity. Otherwise a
    // provider can evade duplicate protection by changing only the deadline.
    let identity = if name == "bash" {
        arguments.get("command").cloned().unwrap_or(Value::Null)
    } else {
        arguments.clone()
    };
    format!("{name}\0{identity}")
}
#[cfg(feature = "tui")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum Answer {
    Yes {
        note: Option<String>,
    },
    Rule {
        note: Option<String>,
    },
    /// Refuse this call; the turn carries on and can propose something else.
    No {
        note: Option<String>,
    },
    /// Refuse this call and end the turn.
    Stop,
}

/// Run a turn on a configured provider, executing the tools it asks for.
///
/// Each round is one request. A round that ends without tool calls is the
/// answer; a round that asks for tools runs the confirmed ones, appends the
/// call and its result to the conversation, and asks again.
///
/// Nothing runs unconfirmed: every call is shown and answered from the
/// keyboard, and a declined call is reported to the model as a failed result
/// rather than hidden, so it can say what it would do instead.
#[cfg(feature = "tui")]
fn user_intent_digest(conversation: &[ModelMessage]) -> arsy_kernel::domain::StateVersion {
    let intent = conversation
        .iter()
        .rev()
        .filter(|message| message.role == ModelRole::User)
        .flat_map(|message| message.content.iter())
        .find_map(|content| match content {
            ModelContent::Text { text } => Some(text.as_bytes()),
            _ => None,
        })
        .unwrap_or_default();
    arsy_kernel::domain::StateVersion::from_digest(Sha256::digest(intent).into())
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn native_turn(
    resolved: &mut provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &mut Vec<ModelMessage>,
    history: &arsy_code::agent::budget::History,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    transcript: &mut tui::Transcript,
    approval: &approval::ApprovalCell,
    hooks: Option<&arsy_code::hook::HookEngine>,
) -> io::Result<Turn> {
    let intent_digest = user_intent_digest(conversation);
    // rather than taken from the last one: an audit that reads a tool-using
    // turn as the price of its final request under-reports what it cost.
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let charge = |outcome: &mut Turn, input: &mut u64, output: &mut u64| {
        *input += outcome.usage["input_tokens"].as_u64().unwrap_or_default();
        *output += outcome.usage["output_tokens"].as_u64().unwrap_or_default();
        if *input > 0 || *output > 0 {
            outcome.usage = json!({"input_tokens": *input, "output_tokens": *output});
        }
    };
    // Successful calls are memoized within this turn. If a provider asks for
    // the exact same effect again, return the first result instead of running
    // it twice or burning all 24 rounds.
    let mut completed_calls = std::collections::HashMap::<String, String>::new();
    let mut changed_files = std::collections::BTreeSet::new();
    let max_rounds = config.max_tool_rounds();
    // Repeating a call that already succeeded is wasted budget, but repeating
    // one that just *failed* is a loop the operator cannot see past the tool
    // cards. Three identical failures in a row is the line: past it the model
    // is told to change approach rather than spend the rest of its budget
    // failing identically.
    const FAILURE_LOOP_LIMIT: usize = 3;
    let mut identical_failures: Option<(String, usize)> = None;
    for round in 0..max_rounds {
        // Before the request, not after: a transcript that has outgrown the
        // window fails at the provider, and the operator is told what was
        // elided rather than watching the turn shrink invisibly.
        report_trim(
            colour,
            &arsy_code::agent::budget::fit(conversation, context_budget(resolved), Some(history)),
        )?;
        let mut outcome = native_status_with_refresh(
            resolved,
            config,
            runtime,
            conversation,
            route,
            effort,
            turn,
            round,
            colour,
            footer,
            keys,
            decoder,
            composer,
            approval,
        )?;
        charge(&mut outcome, &mut input_tokens, &mut output_tokens);
        if outcome.calls.is_empty() || outcome.interrupted || outcome.failure.is_some() {
            outcome.changed_files = changed_files;
            return Ok(outcome);
        }
        // The calls are history now, whatever the operator decides about them:
        // a provider that sent a call and never sees its result rejects the
        // next request.
        let calls = std::mem::take(&mut outcome.calls);
        let mut content: Vec<ModelContent> = Vec::new();
        if !outcome.response.trim().is_empty() {
            content.push(ModelContent::Text {
                text: outcome.response.clone(),
            });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        conversation.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });

        let mut terminal = io::stdout();
        let (results, all_repeated, newly_changed) = run_round_calls(
            &calls,
            runtime,
            intent_digest,
            &mut terminal,
            colour,
            keys,
            decoder,
            approval,
            hooks,
            composer,
            footer,
            transcript,
            &mut completed_calls,
            &mut outcome.interrupted,
        )?;
        changed_files.extend(newly_changed);
        // Read the last failing fingerprint before `results` moves into the
        // conversation below.
        let failed_fingerprint =
            calls
                .iter()
                .zip(results.iter())
                .find_map(|((_, name, arguments), result)| {
                    matches!(result, ModelContent::ToolResult { is_error: true, .. })
                        .then(|| tool_call_fingerprint(name, arguments))
                });
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
        if all_repeated && !calls.is_empty() {
            outcome.response =
                "The requested operation already completed; a repeated tool call was skipped."
                    .to_owned();
            outcome.changed_files = changed_files;
            return Ok(outcome);
        }
        // The response of a round that called tools belongs to the history
        // above, not to the answer this turn returns.
        outcome.response.clear();
        if outcome.interrupted {
            outcome.changed_files = changed_files;
            return Ok(outcome);
        }
        // Three identical failures in a row is a loop, not work: stop the
        // turn with a message that names the loop rather than the provider.
        identical_failures = match (identical_failures, failed_fingerprint) {
            (Some((fingerprint, count)), Some(same)) if fingerprint == same => {
                Some((fingerprint, count + 1))
            }
            (_, Some(fingerprint)) => Some((fingerprint, 1)),
            (_, None) => None,
        };
        if let Some((_, count)) = &identical_failures {
            if *count >= FAILURE_LOOP_LIMIT {
                outcome.failure = Some(format!(
                    "{route} repeated the same failing tool call {count} times — it is stuck in a \
                     loop rather than out of budget; continue with a narrower task"
                ));
                outcome.changed_files = changed_files;
                return Ok(outcome);
            }
        }
        let remaining = max_rounds - (round + 1);
        if remaining > 0 && remaining <= 3 {
            // Told before the budget is gone, not after: a model that knows
            // one round is left can wrap up, while one stopped dead can only
            // be rewound. The note rides on the result just pushed.
            let note = format!(
                "\n\n[SYSTEM: {remaining} tool round(s) remain in this turn. Finish up and give \
                 your final answer now.]"
            );
            if let Some(ModelContent::ToolResult { content, .. }) = conversation
                .last_mut()
                .and_then(|message| message.content.last_mut())
            {
                content.push_str(&note);
            }
        }
    }
    Ok(Turn {
        changed_files,
        failure: Some(format!(
            "{route} asked for tools {max_rounds} times without finishing the turn — the budget \
             is `execution.max_tool_rounds`; raise it, or continue with a narrower task"
        )),
        ..Turn::default()
    })
}

/// Take the terminal's size again, no more than ten times a second.
///
/// Answers whether it was measured on this pass, because the rows on screen
/// were laid out for the size before it.
#[cfg(feature = "tui")]
fn remeasure(
    painter: &Painter<'_>,
    composer: &mut tui::Composer,
    refreshed: &mut std::time::Instant,
) -> bool {
    if refreshed.elapsed() < std::time::Duration::from_millis(100) {
        return false;
    }
    painter.width.set(tui::terminal_width());
    composer.set_height(tui::terminal_rows());
    *refreshed = std::time::Instant::now();
    true
}

/// Start the provider and wire its three streams.
///
/// Stderr and the task being written are each read on their own thread, and
/// the event stream on a third, so the main loop can watch the keyboard while
/// the provider works — which is what lets Esc stop a turn and keeps the
/// composer typeable.
#[cfg(feature = "tui")]
type ProviderStreams = (
    tui::ProviderChild,
    std::sync::mpsc::Receiver<String>,
    std::sync::mpsc::Receiver<io::Result<()>>,
    std::sync::mpsc::Receiver<io::Result<String>>,
);

#[cfg(feature = "tui")]
fn spawn_provider(mut command: std::process::Command, task: &str) -> io::Result<ProviderStreams> {
    // A process group of its own, so a signal aimed at the harness does not also
    // reach the provider child. There is no Windows equivalent to gate on.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut child = tui::ProviderChild(child);
    let mut stderr = child.0.stderr.take().expect("piped stderr is available");
    let (errors, error_output) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = Read::by_ref(&mut stderr).take(8192).read_to_end(&mut bytes);
        let _ = io::copy(&mut stderr, &mut io::sink());
        let _ = errors.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let mut stdin = child.0.stdin.take().expect("piped stdin is available");
    let task = task.to_owned();
    let (sent, input) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sent.send(stdin.write_all(task.as_bytes()));
    });
    // The event stream is read on a thread so the main loop can also watch the
    // key stream: that is what lets Esc or Ctrl-C stop a turn, and what keeps
    // the composer alive and typeable while the provider works.
    let stdout = child.0.stdout.take().expect("piped stdout is available");
    let events = tui::provider_lines(stdout);
    Ok((child, error_output, input, events))
}

#[cfg(feature = "tui")]
struct Streamlined<'a> {
    outcome: &'a mut Turn,
    finished: &'a mut Option<std::time::Instant>,
    stopped_early: &'a mut bool,
    seen_git: &'a mut std::collections::HashSet<String>,
    /// The last row drawn, so an event that renders the same twice is drawn
    /// once.
    last_row: &'a mut Option<String>,
}

/// Read one line from the provider and draw what it says.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn show_event(
    line: &str,
    redactor: &Redactor,
    child: &mut tui::ProviderChild,
    painter: &Painter<'_>,
    terminal: &mut io::Stdout,
    composer: &mut tui::Composer,
    run: Streamlined<'_>,
    colour: bool,
    tick: usize,
) -> io::Result<()> {
    let line = redactor.sanitize(line).map_err(io::Error::other)?;
    let event = serde_json::from_str::<Value>(&line)
        .map_err(|_| io::Error::other("provider emitted invalid JSON"))?;
    absorb_event(&event, run.outcome, run.finished);
    let repeated_git = run.finished.is_none() && repeated_git(&event, run.seen_git);
    if repeated_git {
        *run.stopped_early = true;
        *run.finished = Some(std::time::Instant::now());
        child.stop(false);
        note_repeated_git(run.outcome);
        painter.row(
            terminal,
            composer,
            Some(&tui::tool_result_row(
                colour,
                "git",
                true,
                "repeated successful command skipped",
            )),
            false,
            run.outcome.queued.len(),
            tick,
        )?;
    }
    // A killed provider still flushes buffered events; showing them
    // after the interrupt notice would contradict it.
    if !run.outcome.interrupted && !repeated_git {
        if let Some(row) = tui::render_codex_event(&line, colour) {
            if run.last_row.as_ref() != Some(&row) {
                painter.row(
                    terminal,
                    composer,
                    Some(&row),
                    false,
                    run.outcome.queued.len(),
                    tick,
                )?;
            }
            *run.last_row = Some(row);
        }
    }
    Ok(())
}

/// Say in the answer that a repeated Git command was stopped.
///
/// The turn ends here, so the reason has to reach the model in the answer
/// itself: the row on screen is for the operator, not for the next request.
#[cfg(feature = "tui")]
fn note_repeated_git(outcome: &mut Turn) {
    if !outcome.response.is_empty() {
        outcome.response.push_str("\n\n");
    }
    outcome
        .response
        .push_str("The provider repeated a successful Git command; the duplicate was skipped.");
}

/// Take what an event says about the turn.
///
/// The provider's own words are the answer; a terminal event settles when the
/// turn ended, whatever the process does afterwards.
#[cfg(feature = "tui")]
fn absorb_event(event: &Value, outcome: &mut Turn, finished: &mut Option<std::time::Instant>) {
    if finished.is_none()
        && matches!(
            event["type"].as_str(),
            Some("turn.completed" | "turn.failed")
        )
    {
        *finished = Some(std::time::Instant::now());
    }
    outcome.provider_failed |= event["type"] == "turn.failed";
    if event["type"] == "item.completed" && event["item"]["type"] == "file_change" {
        for path in event["item"]["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|change| change["path"].as_str())
        {
            outcome.changed_files.insert(path.to_owned());
        }
    }
    if event["type"] != "item.completed" || event["item"]["type"] != "agent_message" {
        return;
    }
    if let Some(text) = event["item"]["text"].as_str() {
        if !outcome.response.is_empty() {
            outcome.response.push('\n');
        }
        outcome.response.push_str(text);
    }
}

/// Whether the turn's loop carries on.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Pass {
    Go,
    Stop,
}

/// The clocks a running provider turn watches.
#[cfg(feature = "tui")]
struct Clocks<'a> {
    started: std::time::Instant,
    status: &'a mut Option<std::process::ExitStatus>,
    /// When the process was seen to have left.
    exited: &'a mut Option<std::time::Instant>,
    /// When the provider said the turn was over.
    finished: &'a mut Option<std::time::Instant>,
    last_event: std::time::Instant,
    /// When a stop was asked for, so it can be escalated.
    cancelling: Option<std::time::Instant>,
    stopped_early: &'a mut bool,
}

/// Where the provider's process stands at the top of a pass.
///
/// The turn is over when the provider says it is over. A CLI that lingers
/// after its terminal event — cleaning up a session, flushing telemetry —
/// must not keep the clock running against an answer already on screen.
#[cfg(feature = "tui")]
fn lifecycle(child: &mut tui::ProviderChild, clocks: Clocks<'_>) -> io::Result<Pass> {
    if clocks.status.is_none() {
        *clocks.status = child.0.try_wait()?;
        if clocks.status.is_some() {
            *clocks.exited = Some(std::time::Instant::now());
            child.stop(true);
        }
    }
    if clocks
        .exited
        .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
    {
        return Ok(Pass::Stop);
    }
    // The turn is over when the provider says it is over. A CLI that
    // lingers after its terminal event — cleaning up a session, flushing
    // telemetry — must not keep the clock running against the answer that
    // is already on screen.
    //
    // Trailing rows still land: the stream drains until it has been quiet
    // for 250ms, and no longer than 2 seconds however talkative it stays.
    if clocks.status.is_none()
        && clocks.finished.is_some_and(|at: std::time::Instant| {
            clocks.last_event.elapsed() >= std::time::Duration::from_millis(250)
                || at.elapsed() >= std::time::Duration::from_secs(2)
        })
    {
        *clocks.stopped_early = true;
        child.stop(false);
        return Ok(Pass::Stop);
    }
    if clocks.started.elapsed() >= std::time::Duration::from_secs(300) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "provider exceeded the 300-second turn deadline",
        ));
    }
    if clocks
        .cancelling
        .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
    {
        child.stop(true);
        *clocks.status = Some(child.0.wait()?);
        return Ok(Pass::Stop);
    }
    Ok(Pass::Go)
}

/// The call a live view is watching.
#[cfg(feature = "tui")]
struct LiveCall<'a> {
    name: &'a str,
    /// Only a process can be cancelled part way; everything else runs to its
    /// own end and Ctrl-C would leave the workspace half changed.
    cancellable: bool,
    operation_id: arsy_kernel::domain::OperationId,
}

/// Take the keys pressed while a call runs, answering whether it was
/// cancelled.
///
/// `e` toggles how much of the output is shown. Every other key belongs to
/// the composer and is read once the call is done.
#[cfg(feature = "tui")]
fn absorb_live_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    terminal: &mut io::Stdout,
    call: LiveCall<'_>,
    drawn_rows: &mut usize,
    expanded: &mut bool,
) -> io::Result<bool> {
    let mut cancelled = false;
    for key in keys.try_iter().filter_map(|byte| decoder.feed(byte)) {
        match key {
            tui::Key::Interrupt if call.cancellable => {
                arsy_code::process::cancel(call.operation_id);
                if *drawn_rows > 0 {
                    write!(terminal, "\x1b[{}A\r\x1b[J", drawn_rows)?;
                    *drawn_rows = 0;
                }
                write!(terminal, "\r\x1b[K  ✦ Cancelling {}…\n", call.name)?;
                terminal.flush()?;
                cancelled = true;
            }
            tui::Key::Char('e' | 'E') => *expanded = !*expanded,
            _ => {}
        }
    }
    Ok(cancelled)
}

/// Take whatever a running command has printed since the last pass.
///
/// The tail is what a reader needs while it runs, so the buffer is capped and
/// the oldest output is dropped rather than growing without bound.
#[cfg(feature = "tui")]
fn absorb_output(output: &std::sync::mpsc::Receiver<String>, live: &mut String) {
    /// What is kept of a long-running command's output.
    const KEEP_BYTES: usize = 16_384;

    live.extend(output.try_iter());
    if live.len() > KEEP_BYTES {
        let oldest = live.len() - KEEP_BYTES;
        live.drain(..oldest);
    }
}

/// What a key press during a provider turn can reach.
#[cfg(feature = "tui")]
struct Keyboard<'a> {
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    composer: &'a mut tui::Composer,
    approval: &'a approval::ApprovalCell,
}

/// The turn those keys can change.
#[cfg(feature = "tui")]
struct Turning<'a> {
    outcome: &'a mut Turn,
    cancelling: &'a mut Option<std::time::Instant>,
    last_key: &'a mut std::time::Instant,
}

/// Take the keys waiting, without blocking on the next one.
///
/// Bounded per pass so a held key cannot starve the event stream: whatever is
/// still waiting is read on the pass after this one.
///
/// Answers whether anything typed changed what is on screen.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn provider_keys(
    board: Keyboard<'_>,
    terminal: &mut io::Stdout,
    painter: &Painter<'_>,
    child: &mut tui::ProviderChild,
    turning: Turning<'_>,
    colour: bool,
    tick: usize,
) -> io::Result<bool> {
    let mut typed = false;
    for _ in 0..256 {
        let byte = match board.keys.try_recv() {
            Ok(byte) => byte,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                turning.outcome.quit = true;
                stop_turn(turning.outcome, child, turning.cancelling);
                break;
            }
        };
        *turning.last_key = std::time::Instant::now();
        let Some(key) = board.decoder.feed(byte) else {
            continue;
        };
        // While the provider is running, Interrupt always means the turn,
        // never the composer or the session — and it drops a queued
        // follow-up, which was only queued to run after this turn.
        if key == tui::Key::Interrupt {
            if stop_turn(turning.outcome, child, turning.cancelling) {
                painter.row(
                    terminal,
                    board.composer,
                    Some(&tui::interrupted_row(colour)),
                    true,
                    0,
                    tick,
                )?;
            }
            continue;
        }
        match board.composer.press(key) {
            // Shift+Tab is an immediate mode change, not a follow-up task.
            // Keeping it out of the queue prevents a drafted chat line from
            // being answered as if it were a second user message.
            tui::Action::CycleMode => {
                cycle_approval_mode(board.approval);
                typed = true;
            }
            // Mid-turn the line belongs to the operator's draft, so `e`
            // stays text rather than expanding anything.
            tui::Action::Expand => typed = true,
            // A line sent while the provider is busy runs as soon as this
            // turn ends, rather than being dropped or blocking.
            tui::Action::Submit(line) if !line.trim().is_empty() => {
                if turning.outcome.queued.len() < 16 {
                    // Queued, not dropped: the row below says it was
                    // taken, and the turn that follows this one runs it.
                    turning.outcome.queued.push_back(line);
                    painter.row(
                        terminal,
                        board.composer,
                        Some("  Follow-up queued."),
                        turning.cancelling.is_some(),
                        turning.outcome.queued.len(),
                        tick,
                    )?;
                } else {
                    board.composer.restore(line);
                    painter.row(
                        terminal,
                        board.composer,
                        Some("  Queue full; draft retained."),
                        turning.cancelling.is_some(),
                        turning.outcome.queued.len(),
                        tick,
                    )?;
                }
            }
            tui::Action::Submit(_) => typed = true,
            tui::Action::Quit => {
                turning.outcome.quit = true;
                stop_turn(turning.outcome, child, turning.cancelling);
            }
            tui::Action::Redraw => typed = true,
            tui::Action::None => {}
        }
    }
    Ok(typed)
}

/// Draws the rows a provider turn produces, above the live composer.
#[cfg(feature = "tui")]
pub(crate) struct Painter<'a> {
    pub(crate) colour: bool,
    pub(crate) footer: &'a str,
    /// Re-measured on the resize tick rather than per row.
    pub(crate) width: std::cell::Cell<usize>,
    pub(crate) started: std::time::Instant,
}

#[cfg(feature = "tui")]
impl Painter<'_> {
    /// One row, or none — either way the status under it is repainted.
    ///
    /// The composer is torn down and drawn again around each row, so the input
    /// block is never overwritten by what lands above it.
    pub(crate) fn row(
        &self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        row: Option<&str>,
        cancelling: bool,
        queued: usize,
        tick: usize,
    ) -> io::Result<()> {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(block_gap(row));
            frame.push_str(row);
            frame.push('\n');
        }
        let phase = if cancelling {
            tui::TurnPhase::Cancelling
        } else {
            tui::TurnPhase::Working
        };
        let status = tui::turn_status(self.colour, phase, self.started.elapsed(), tick, queued);
        frame.push_str(&composer.render_turn(self.width.get(), self.colour, &status, self.footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    }
}

/// What the provider's exit says about the turn, once the turn itself is done.
///
/// A zero process exit must not mask a turn the provider reported as failed,
/// so the event stream is read before the exit status.
#[cfg(feature = "tui")]
fn verdict(
    child: &mut tui::ProviderChild,
    route: &tui::ModelRoute,
    status: Option<std::process::ExitStatus>,
    stopped_early: bool,
    outcome: &Turn,
) -> io::Result<Option<String>> {
    let status = match status {
        Some(status) => status,
        // The turn ended before the process did, so the process is asked to
        // leave and then made to: waiting on a CLI that ignores the signal is
        // the hang this exit was added to avoid.
        None if stopped_early => reap(child)?,
        None => child.0.wait()?,
    };
    Ok(if outcome.provider_failed {
        Some(format!("{route} reported a failed turn"))
    // A signal ARSY sent after a completed turn is its own exit code, not a
    // verdict on the turn the provider already reported.
    } else if status.success() || stopped_early {
        None
    } else {
        Some(format!("{route} exited with status {status}"))
    })
}

/// Wait briefly for a provider asked to leave, then make it.
#[cfg(feature = "tui")]
fn reap(child: &mut tui::ProviderChild) -> io::Result<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            child.stop(true);
            return child.0.wait();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The error for a provider that closed its stream without ever saying the
/// turn ended, carrying whatever it wrote to stderr.
#[cfg(feature = "tui")]
fn silent_provider(
    errors: &std::sync::mpsc::Receiver<String>,
    redactor: &Redactor,
) -> io::Result<io::Error> {
    let detail = errors
        .recv_timeout(std::time::Duration::from_millis(100))
        .unwrap_or_default();
    let detail = redactor.sanitize(&detail).map_err(io::Error::other)?;
    Ok(io::Error::other(format!(
        "provider closed its stream without a terminal turn event: {}",
        terminal_text(detail.trim())
    )))
}

/// Whether this event is a Git command that already succeeded this turn.
///
/// A provider that repeats a push or a commit would run it twice, so the
/// duplicate is caught at its start event — before it gets a second chance —
/// which means the set is filled by the completions that came before it.
#[cfg(feature = "tui")]
fn repeated_git(event: &Value, seen: &mut std::collections::HashSet<String>) -> bool {
    if event["item"]["type"] != "command_execution" {
        return false;
    }
    let Some(command) = event["item"]
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| command.contains("git "))
    else {
        return false;
    };
    match event["type"].as_str() {
        Some("item.started") => seen.contains(command),
        Some("item.completed") if event["item"]["exit_code"].as_i64() == Some(0) => {
            !seen.insert(command.to_owned())
        }
        _ => false,
    }
}

/// Stop the running turn.
///
/// The queue goes with it: a follow-up was only queued to run after this turn,
/// and the operator stopping the turn is not asking for the next one. The
/// moment the stop began is kept so an unresponsive provider can be escalated
/// from a polite stop to a kill.
///
/// Answers whether this call was the one that started the stop, because that
/// is when the row saying so is drawn — a second Ctrl-C must not draw it again.
#[cfg(feature = "tui")]
fn stop_turn(
    outcome: &mut Turn,
    child: &mut tui::ProviderChild,
    cancelling: &mut Option<std::time::Instant>,
) -> bool {
    outcome.queued.clear();
    outcome.interrupted = true;
    if cancelling.is_some() {
        return false;
    }
    *cancelling = Some(std::time::Instant::now());
    child.stop(false);
    true
}

/// What a streaming round has put on screen so far.
///
/// Reasoning and the answer hold separate buffers and separate boxes, so the
/// verbose stream reads as distinct parts of the turn rather than one grey
/// blur. The live part of the answer is reparsed as Markdown on every frame,
/// keeping the live response block consistent with the settled one.
///
/// Answer text is not drawn the moment a delta lands: deltas arrive in
/// whatever bursts the network delivers, so they are queued and revealed a few
/// words per frame instead, which is what makes the answer read as typed.
#[cfg(feature = "tui")]
#[derive(Default)]
pub(crate) struct Streaming {
    /// Revealed Markdown that is still live, below whatever has settled.
    response: String,
    /// Received but not yet revealed.
    pending: String,
    thinking: String,
    thinking_open: bool,
    /// Rows drawn for the current Markdown response block.
    live_lines: usize,
    /// Part of this answer already settled into scrollback, so the live rest
    /// draws without a second `✦`.
    continued: bool,
    last_frame: Option<std::time::Instant>,
    /// When the text now in `pending` started waiting for a word boundary.
    waiting_since: Option<std::time::Instant>,
    /// `(width, rows, read at)`. Each read spawns `stty`, which is too dear
    /// to do on every frame.
    size: Option<(usize, usize, std::time::Instant)>,
}

/// How often queued answer text is revealed.
#[cfg(feature = "tui")]
pub(crate) const FRAME: std::time::Duration = std::time::Duration::from_millis(25);

/// Frames a backlog is spread over, so the reveal never trails the provider by
/// more than about `FRAME * CATCH_UP_FRAMES`.
#[cfg(feature = "tui")]
const CATCH_UP_FRAMES: usize = 10;

/// How long queued text may wait for a word boundary before it is revealed
/// anyway: `FRAME * CATCH_UP_FRAMES`, the same lag the pacing allows. A
/// script written without spaces may never send one.
#[cfg(feature = "tui")]
const STALL: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(feature = "tui")]
impl Streaming {
    /// Draw reasoning as it streams, opening its box once real content has
    /// arrived.
    ///
    /// A stream that opens with an empty or whitespace-only delta and never
    /// sends anything else must not leave a bare, empty box on screen: the
    /// announcement is held back until there is something to announce.
    pub(crate) fn reason(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        let width = tui::terminal_width();
        self.thinking.push_str(text);
        if !self.thinking_open {
            if self.thinking.trim().is_empty() {
                return Ok(());
            }
            self.thinking_open = true;
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_top(width, colour),
            )?;
        }
        for line in drain_lines(&mut self.thinking) {
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_row(width, colour, &line),
            )?;
        }
        Ok(())
    }

    /// Queue answer text for [`Self::pace`] to reveal. Answer text closes the
    /// reasoning box first, so the prose never starts inside it.
    pub(crate) fn answer(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        self.pending.push_str(text);
        self.waiting_since
            .get_or_insert_with(std::time::Instant::now);
        Ok(())
    }

    /// Whether answer text is still waiting to be revealed.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Reveal the next few queued words if a frame is due, returning whether
    /// anything was drawn.
    pub(crate) fn pace(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<bool> {
        if self.pending.is_empty() || self.last_frame.is_some_and(|last| last.elapsed() < FRAME) {
            return Ok(false);
        }
        let take = match reveal_len(&self.pending) {
            0 if self
                .waiting_since
                .is_some_and(|since| since.elapsed() >= STALL) =>
            {
                self.pending.len()
            }
            0 => return Ok(false),
            take => take,
        };
        self.response.extend(self.pending.drain(..take));
        let now = std::time::Instant::now();
        self.last_frame = Some(now);
        self.waiting_since = (!self.pending.is_empty()).then_some(now);
        self.redraw(terminal, composer, colour, footer, status)?;
        Ok(true)
    }

    /// Redraw the live block, first settling into scrollback whatever part of
    /// it would no longer fit on screen: cursor-up cannot reach a row that has
    /// scrolled off, so a block taller than the screen could not be erased.
    fn redraw(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<()> {
        let (width, rows) = self.size();
        let composer_rows = composer
            .render_turn(width, colour, status, footer)
            .lines()
            .count();
        let room = rows.saturating_sub(composer_rows + 2).max(1);
        while self.block(width, colour, &self.response).lines().count() > room {
            // ponytail: a single line taller than the screen has no split
            // point and still overflows; a full-screen renderer would fix it.
            let Some((settled, rest, paragraph)) = settle_split(&self.response) else {
                break;
            };
            if rest.len() >= self.response.len() {
                break;
            }
            self.settle(
                terminal, composer, colour, footer, status, width, &settled, paragraph,
            )?;
            self.response = rest;
        }
        if self.response.trim().is_empty() {
            if self.live_lines > 0 {
                erase_live_response(terminal, composer, self.live_lines)?;
                self.live_lines = 0;
            }
            return Ok(());
        }
        let block = self.block(width, colour, &self.response);
        self.live_lines = redraw_live_response(
            terminal,
            composer,
            colour,
            footer,
            status,
            width,
            &block,
            self.live_lines,
        )?;
        Ok(())
    }

    /// The terminal size, reread at most every quarter second.
    fn size(&mut self) -> (usize, usize) {
        match self.size {
            Some((width, rows, read)) if read.elapsed() < std::time::Duration::from_millis(250) => {
                (width, rows)
            }
            _ => {
                let (width, rows) = (tui::terminal_width(), tui::terminal_rows());
                self.size = Some((width, rows, std::time::Instant::now()));
                (width, rows)
            }
        }
    }

    fn block(&self, width: usize, colour: bool, text: &str) -> String {
        if self.continued {
            tui::assistant_continuation(width, colour, text)
        } else {
            tui::assistant_block(width, colour, text)
        }
    }

    /// Replace the live block with `text` for good. `paragraph` leaves the
    /// blank line a paragraph break would have drawn, so the rest of the
    /// answer does not butt up against it.
    #[allow(clippy::too_many_arguments)]
    fn settle(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
        width: usize,
        text: &str,
        paragraph: bool,
    ) -> io::Result<()> {
        let mut frame = composer.clear();
        for _ in 0..self.live_lines {
            frame.push_str("\x1b[1A\r\x1b[K");
        }
        self.live_lines = 0;
        if !text.trim().is_empty() {
            let block = self.block(width, colour, text);
            if !self.continued {
                frame.push_str(block_gap(&block));
            }
            frame.push_str(&block);
            frame.push('\n');
            if paragraph {
                frame.push_str(modern_gap());
            }
            self.continued = true;
        }
        frame.push_str(&composer.render_turn(width, colour, status, footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    }

    /// Close the round: reveal whatever is still queued, finish whatever box
    /// is open and settle the answer.
    pub(crate) fn close(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        let pending = std::mem::take(&mut self.pending);
        self.response.push_str(&pending);
        if self.response.trim().is_empty() {
            return self.abandon(terminal, composer);
        }
        self.redraw(terminal, composer, colour, footer, status)?;
        let response = std::mem::take(&mut self.response);
        let (width, _) = self.size();
        self.settle(
            terminal, composer, colour, footer, status, width, &response, false,
        )
    }

    /// Erase the live block, leaving what already settled.
    pub(crate) fn abandon(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
    ) -> io::Result<()> {
        if self.live_lines > 0 {
            erase_live_response(terminal, composer, self.live_lines)?;
            self.live_lines = 0;
        }
        Ok(())
    }

    /// Close the reasoning box if it is open, flushing the line it was part
    /// way through.
    pub(crate) fn close_thinking(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &str,
        status: &str,
    ) -> io::Result<()> {
        if !self.thinking_open {
            return Ok(());
        }
        self.thinking_open = false;
        let width = tui::terminal_width();
        if !self.thinking.trim().is_empty() {
            let line = std::mem::take(&mut self.thinking);
            stream_row(
                terminal,
                composer,
                colour,
                footer,
                status,
                &tui::thinking_box_row(width, colour, &line),
            )?;
        }
        stream_row(
            terminal,
            composer,
            colour,
            footer,
            status,
            &tui::thinking_box_bottom(width, colour),
        )
    }
}

/// Take the finished lines out of a streaming buffer, leaving whatever part of
/// the next one has arrived.
///
/// One scan for the last break rather than one per line: a buffer is appended
/// to on every delta, and re-scanning it from the front for each line it holds
/// is quadratic in a long answer.
#[cfg(feature = "tui")]
fn drain_lines(buffer: &mut String) -> Vec<String> {
    let Some(last) = buffer.rfind('\n') else {
        return Vec::new();
    };
    let complete: String = buffer.drain(..=last).collect();
    complete.split_inclusive('\n').map(str::to_owned).collect()
}

/// How many bytes of queued answer text the next frame reveals.
///
/// Whole words only, each with the whitespace after it: a word is complete
/// once that whitespace has arrived, so a trailing fragment waits for the
/// rest of itself. One word per frame while the backlog is small, a tenth of
/// it once it grows, so a fast provider is never left behind.
///
/// Scripts written without spaces (CJK, kana, hangul) have no whitespace to
/// wait for, so each of their characters is a word of its own. Anything else
/// that never sends a boundary is released by the stall in `pace`.
#[cfg(feature = "tui")]
pub(crate) fn reveal_len(pending: &str) -> usize {
    fn push(ends: &mut Vec<usize>, end: usize) {
        if ends.last() != Some(&end) {
            ends.push(end);
        }
    }
    let mut ends = Vec::new();
    let mut seen_word = false;
    let mut after_space = false;
    for (index, character) in pending.char_indices() {
        if character.is_whitespace() {
            after_space = seen_word;
            continue;
        }
        let unspaced = is_unspaced(character);
        if after_space || (unspaced && seen_word) {
            push(&mut ends, index);
        }
        if unspaced {
            push(&mut ends, index + character.len_utf8());
        }
        seen_word = true;
        after_space = false;
    }
    if after_space {
        ends.push(pending.len());
    }
    if ends.is_empty() {
        return 0;
    }
    let words = (ends.len() / CATCH_UP_FRAMES).clamp(1, ends.len());
    ends[words - 1]
}

/// A character from a script that does not put spaces between words.
#[cfg(feature = "tui")]
fn is_unspaced(character: char) -> bool {
    matches!(
        character,
        '\u{1100}'..='\u{11FF}'     // Hangul Jamo
            | '\u{2E80}'..='\u{9FFF}' // CJK radicals, punctuation, kana, ideographs
            | '\u{A960}'..='\u{A97F}' // Hangul Jamo Extended-A
            | '\u{AC00}'..='\u{D7AF}' // Hangul syllables
            | '\u{F900}'..='\u{FAFF}' // CJK compatibility ideographs
            | '\u{FF00}'..='\u{FFEF}' // Halfwidth and fullwidth forms
            | '\u{20000}'..='\u{3FFFF}' // CJK extensions B onwards
    )
}

/// Where a live answer can be cut so its head settles into scrollback:
/// `(settled, rest, paragraph)`.
///
/// The last blank line outside a code fence, else the last line break. A cut
/// inside a fence closes it in the settled half and reopens it in the rest,
/// so both halves still render as code. `paragraph` says the cut was a
/// paragraph break.
#[cfg(feature = "tui")]
pub(crate) fn settle_split(text: &str) -> Option<(String, String, bool)> {
    let mut fence: Option<String> = None;
    let mut paragraph = None;
    let mut line_break = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        offset = end;
        let trimmed = line.trim();
        let opened_here = if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if fence.take().is_none() {
                fence = Some(trimmed.to_owned());
                true
            } else {
                false
            }
        } else {
            false
        };
        if !line.ends_with('\n') || opened_here {
            continue;
        }
        line_break = Some((end, fence.clone()));
        if fence.is_none() && trimmed.is_empty() {
            paragraph = Some(end);
        }
    }
    if let Some(end) = paragraph {
        return Some((text[..end].to_owned(), text[end..].to_owned(), true));
    }
    let (end, fence) = line_break?;
    let (mut settled, mut rest) = (text[..end].to_owned(), text[end..].to_owned());
    if let Some(opener) = fence {
        let marker: String = opener
            .chars()
            .take_while(|character| *character == '`' || *character == '~')
            .collect();
        settled.push_str(&marker);
        settled.push('\n');
        rest = format!("{opener}\n{rest}");
    }
    Some((settled, rest, false))
}

/// A blank line above a block, so the transcript reads as a sequence of steps
/// rather than one wall of text.
///
/// A block is anything that occupies more than one row — a tool card, an
/// answer, a plan. Single rows stay tight against each other, which is how the
/// mockup draws a run of them, and because each block brings its own gap two
/// in a row are separated by exactly one blank line rather than two.
///
/// Modern only. The classic style's spacing is what an operator who chose it
/// already has, and widening it is not a thing they asked for.
pub(crate) fn block_gap(row: &str) -> &'static str {
    if row.contains('\n') {
        modern_gap()
    } else {
        ""
    }
}

/// The separator itself, for something already known to be a block.
pub(crate) fn modern_gap() -> &'static str {
    if tui::modern_style() {
        "\n"
    } else {
        ""
    }
}

/// One finished row above the composer, with the status redrawn under it.
#[cfg(feature = "tui")]
pub(crate) fn stream_row(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &str,
    status: &str,
    row: &str,
) -> io::Result<()> {
    let mut frame = composer.clear();
    frame.push_str(block_gap(row));
    frame.push_str(row);
    frame.push('\n');
    frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
    write!(terminal, "{frame}").and_then(|()| terminal.flush())
}

/// What the keys pressed while a round streams amount to.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Typed {
    /// Nothing that changes what is on screen.
    Quiet,
    Redraw,
    Interrupted,
}

/// Take every key waiting, without blocking on the next one.
///
/// A turn is streaming while this runs, so the composer stays live: a
/// follow-up can be queued, the approval mode can change, and the turn can be
/// stopped, all without waiting for the provider to finish.
#[cfg(feature = "tui")]
pub(crate) fn drain_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
    outcome: &mut Turn,
) -> Typed {
    let mut typed = Typed::Quiet;
    while let Ok(byte) = keys.try_recv() {
        let Some(key) = decoder.feed(byte) else {
            continue;
        };
        if key == tui::Key::Interrupt {
            outcome.queued.clear();
            outcome.interrupted = true;
            return Typed::Interrupted;
        }
        match composer.press(key) {
            // Shift+Tab changes authority immediately; it never becomes a
            // model prompt or a queued follow-up.
            tui::Action::CycleMode => {
                cycle_approval_mode(approval);
                typed = Typed::Redraw;
            }
            // Bounded as on the Codex route, so a held Enter cannot grow the
            // queue without limit; past the bound the draft is handed back.
            tui::Action::Submit(line) if !line.trim().is_empty() => {
                if outcome.queued.len() < 16 {
                    outcome.queued.push_back(line);
                } else {
                    composer.restore(line);
                }
            }
            tui::Action::Expand | tui::Action::Submit(_) | tui::Action::Redraw => {
                typed = Typed::Redraw
            }
            tui::Action::Quit => outcome.quit = true,
            tui::Action::None => {}
        }
    }
    typed
}

/// The request one round of a turn sends.
///
/// Built per round rather than captured once: the instructions are discovered
/// by walking the workspace, and an AGENTS.md the turn just edited is the one
/// the next round should read.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn round_request(
    resolved: &provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
) -> io::Result<CanonicalModelRequest> {
    Ok(CanonicalModelRequest {
        model: ModelKey {
            provider: route.provider.clone(),
            model: route.model.clone(),
        },
        system: system_prompt(
            runtime.workspace(),
            config,
            &route.provider,
            &route.model,
            runtime.execution_mode(),
        ),
        messages: conversation.to_vec(),
        tools: runtime.schemas(),
        max_output_tokens: resolved.endpoint.max_output_tokens,
        effort,
        // One turn can take several requests, one per round of tool calls. The
        // round is part of the key, because a retry must repeat its own
        // request rather than collapse into the one before it.
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(format!("{turn}-{round}"))
            .map_err(io::Error::other)?,
    })
}

/// Read the provider's stream on its own thread, as the rows the turn draws.
///
/// The provider's events are turned into rows here rather than at the far end,
/// so the drawing loop waits on one channel and nothing else.
#[cfg(feature = "tui")]
fn spawn_stream(
    provider: Arc<dyn arsy_kernel::provider::ModelProvider>,
    request: CanonicalModelRequest,
) -> std::sync::mpsc::Receiver<Result<Streamed, arsy_kernel::provider::ProviderError>> {
    let (rows, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = match arsy_kernel::provider::stream_with_retry(
            provider.as_ref(),
            &request,
            &mut std::thread::sleep,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = rows.send(Err(error));
                return;
            }
        };
        for event in stream {
            let Some(message) = streamed(event) else {
                continue;
            };
            let failed = message.is_err();
            if rows.send(message).is_err() || failed {
                return;
            }
        }
    });
    events
}

/// The row a provider event draws, or `None` for an event the turn does not
/// show.
#[cfg(feature = "tui")]
fn streamed(
    event: Result<ModelEvent, arsy_kernel::provider::ProviderError>,
) -> Option<Result<Streamed, arsy_kernel::provider::ProviderError>> {
    Some(match event {
        Ok(ModelEvent::TextDelta { text }) => Ok(Streamed::Text(text)),
        Ok(ModelEvent::ThinkingDelta { text }) => Ok(Streamed::Thinking(text)),
        Ok(ModelEvent::Usage {
            input_tokens,
            output_tokens,
        }) => Ok(Streamed::Usage {
            input_tokens,
            output_tokens,
        }),
        Ok(ModelEvent::ToolCallCompleted {
            id,
            name,
            arguments,
            ..
        }) => Ok(Streamed::Tool {
            id,
            name,
            arguments,
        }),
        Ok(_) => return None,
        Err(error) => Err(error),
    })
}

/// Say what a context trim removed, when it removed anything.
///
/// A transcript that has outgrown the window fails at the provider, so the
/// operator is told what was elided rather than watching the turn shrink
/// invisibly.
#[cfg(feature = "tui")]
fn report_trim(colour: bool, trimmed: &arsy_code::agent::budget::Trimmed) -> io::Result<()> {
    if !trimmed.changed() {
        return Ok(());
    }
    let mut terminal = io::stdout();
    writeln!(
        terminal,
        "{}",
        tui::tool_result_row(
            colour,
            "context",
            true,
            &format!(
                "elided {} tool result(s) and compacted {} earlier message(s) to stay \
                 within {} tokens",
                trimmed.elided, trimmed.summarized, trimmed.after
            )
        )
    )?;
    terminal.flush()
}

/// The call being answered.
#[cfg(feature = "tui")]
struct Call<'a> {
    name: &'a str,
    arguments: &'a Value,
    /// Identifies the effect, so an identical later call can be answered from
    /// this one's result.
    fingerprint: String,
}

/// What answering a call is allowed to touch.
#[cfg(feature = "tui")]
struct Answering<'a> {
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    approval: &'a approval::ApprovalCell,
    completed: &'a mut std::collections::HashMap<String, String>,
    interrupted: &'a mut bool,
    hooks: Option<&'a arsy_code::hook::HookEngine>,
    /// The input line, drawn under a running call's card rather than replaced
    /// by it: what the operator types while a tool runs is the next turn.
    composer: &'a mut tui::Composer,
    footer: &'a str,
}

#[cfg(feature = "tui")]
struct CallResult {
    output: String,
    is_error: bool,
    metadata: Value,
    changed_files: Vec<String>,
    duration: std::time::Duration,
}

/// Run one call and turn what happened into the result the provider is sent.
#[cfg(feature = "tui")]
fn run_call(
    runtime: &arsy_code::agent::ToolRuntime,
    intent_digest: arsy_kernel::domain::StateVersion,
    terminal: &mut io::Stdout,
    colour: bool,
    summary: &str,
    call: Call<'_>,
    answering: Answering<'_>,
) -> io::Result<CallResult> {
    // A new effect can invalidate an earlier read or command result, so only
    // reuse calls until the next effectful call.
    if !runtime.is_observational(call.name, call.arguments) {
        answering.completed.clear();
    }
    match execute_call(
        runtime,
        intent_digest,
        terminal,
        colour,
        call.name,
        call.arguments,
        summary,
        answering.keys,
        answering.decoder,
        answering.approval,
        answering.hooks,
        answering.composer,
        answering.footer,
    )? {
        Executed::Answered(mut result) => {
            if !result.changed_files.is_empty() {
                result.output.push_str("\nChanged files:\n");
                for path in &result.changed_files {
                    result.output.push_str(&format!("  • {path}\n"));
                }
            }
            if result.success {
                answering
                    .completed
                    .insert(call.fingerprint, result.output.clone());
            }
            Ok(CallResult {
                output: result.output,
                is_error: !result.success,
                metadata: result.metadata,
                changed_files: result.changed_files,
                duration: result.duration,
            })
        }
        Executed::Stopped => {
            *answering.interrupted = true;
            writeln!(terminal, "{}", tui::interrupted_row(colour))?;
            Ok(CallResult {
                output: "The operator stopped the turn.".to_owned(),
                is_error: true,
                metadata: Value::Null,
                changed_files: Vec::new(),
                duration: std::time::Duration::ZERO,
            })
        }
    }
}

/// What happened to one tool call.
#[cfg(feature = "tui")]
enum Executed {
    Answered(arsy_code::agent::ToolResult),
    Stopped,
}

/// Decide, confirm if the decision says to, and run.
///
/// Policy is asked first, so the operator is only interrupted for calls that
/// actually need a human: a read policy already allows runs without a prompt,
/// and a call policy denies is refused without one. That is the difference
/// between an approval and a habit — an operator asked to confirm every read
/// stops reading the prompts.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn execute_call(
    runtime: &arsy_code::agent::ToolRuntime,
    intent_digest: arsy_kernel::domain::StateVersion,
    terminal: &mut io::Stdout,
    colour: bool,
    name: &str,
    arguments: &Value,
    summary: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    approval: &approval::ApprovalCell,
    hooks: Option<&arsy_code::hook::HookEngine>,
    composer: &mut tui::Composer,
    footer: &str,
) -> io::Result<Executed> {
    let started = std::time::Instant::now();
    let mut notes = Vec::new();
    let (arguments, injected) =
        match hooks.map(|hooks| hook_before_call(hooks, name, arguments, &mut notes)) {
            None => (arguments.clone(), Vec::new()),
            Some(hooked) => {
                let asking = Asking {
                    name,
                    arguments,
                    summary,
                    keys,
                    decoder: &mut *decoder,
                    approval,
                };
                match hooked_arguments(terminal, colour, hooked, asking, &notes)? {
                    Ok(run) => run,
                    Err(executed) => return Ok(executed),
                }
            }
        };
    let arguments = &arguments;
    let request = match runtime.prepare(name, arguments) {
        Ok(request) => request,
        Err(failure) => return Ok(Executed::Answered(*failure)),
    };
    let authorization = runtime.authorize(&request);
    let safety = (approval.get() == approval::ApprovalMode::Auto
        && !matches!(&authorization, arsy_code::agent::Authorization::Denied(_)))
    .then(|| {
        runtime.review_auto(
            &request,
            intent_digest,
            arsy_kernel::safety::TrustState::Trusted,
            false,
        )
    });
    let (grants, approval_note) = match authorize(
        terminal,
        colour,
        Asking {
            name,
            arguments,
            summary,
            keys,
            decoder,
            approval,
        },
        authorization,
        safety.as_ref(),
    )? {
        Granted::Run { grants, note } => (grants, note),
        Granted::Refused(result) => return Ok(Executed::Answered(*result)),
        Granted::Stopped => return Ok(Executed::Stopped),
    };
    let (mut result, cancelled) = dispatch_tool_live(
        terminal, colour, runtime, name, &request, &grants, started, summary, keys, decoder,
        composer, footer,
    )?;
    if cancelled {
        return Ok(Executed::Stopped);
    }
    if let Some(note) = approval_note {
        result.output = format!("{}\nOperator note: {note}", result.output);
    }
    if let Some(hooks) = hooks {
        notes.clear();
        hook_after_call(hooks, name, &injected, &mut result, &mut notes);
        hook_notes(terminal, colour, &notes)?;
    }
    Ok(Executed::Answered(result))
}

/// The arguments a call runs with and what hooks injected for the model.
#[cfg(feature = "tui")]
type HookedRun = (Value, Vec<(String, String)>);

/// What `before_operation` leaves the interactive turn to run.
///
/// Unlike a scripted run, this surface has an operator, so a hook that wants
/// one asked gets the approval dialog rather than a refusal.
#[cfg(feature = "tui")]
fn hooked_arguments(
    terminal: &mut io::Stdout,
    colour: bool,
    hooked: HookedCall,
    asking: Asking<'_>,
    notes: &[String],
) -> io::Result<Result<HookedRun, Executed>> {
    hook_notes(terminal, colour, notes)?;
    let (arguments, injected, reason) = match hooked {
        HookedCall::Refused(reason) => {
            return Ok(Err(Executed::Answered(hook_refused(asking.name, reason))))
        }
        HookedCall::Run {
            arguments,
            injected,
            approval,
        } => (arguments, injected, approval),
    };
    let Some(reason) = reason else {
        return Ok(Ok((arguments, injected)));
    };
    let facts = ApprovalFacts {
        name: asking.name.to_owned(),
        summary: asking.summary.to_owned(),
        effect: format!("{} · {}", asking.name, asking.summary),
        scope: asking.summary.to_owned(),
        reversibility: "not reported by hook".to_owned(),
        reason: format!("a hook asks for approval: {reason}"),
        rule_approval: false,
    };
    let answer = confirm_tool(
        terminal,
        colour,
        &facts,
        format_tool_preview(asking.name, &arguments),
        asking.keys,
        asking.decoder,
        asking.approval,
    )?;
    Ok(match answer {
        Answer::Yes { .. } | Answer::Rule { .. } => Ok((arguments, injected)),
        Answer::No { .. } => Err(Executed::Answered(hook_refused(
            asking.name,
            "The operator declined the call a hook asked about.".to_owned(),
        ))),
        Answer::Stop => Err(Executed::Stopped),
    })
}

/// What the hooks said about a call, as dim rows above it.
#[cfg(feature = "tui")]
fn hook_notes(terminal: &mut io::Stdout, colour: bool, notes: &[String]) -> io::Result<()> {
    notes
        .iter()
        .try_for_each(|note| writeln!(terminal, "{}", tui::hook_note_row(colour, note)))
}

/// What deciding a call needs in order to ask about it.
#[cfg(feature = "tui")]
struct Asking<'a> {
    name: &'a str,
    arguments: &'a Value,
    summary: &'a str,
    keys: &'a std::sync::mpsc::Receiver<u8>,
    decoder: &'a mut tui::Keys,
    approval: &'a approval::ApprovalCell,
}

#[cfg(feature = "tui")]
struct ApprovalFacts {
    name: String,
    summary: String,
    effect: String,
    scope: String,
    reversibility: String,
    reason: String,
    rule_approval: bool,
}

#[cfg(feature = "tui")]
fn policy_approval_facts(
    name: &str,
    summary: &str,
    authorization: &arsy_code::agent::Authorization,
) -> ApprovalFacts {
    use arsy_code::agent::Authorization;
    let Authorization::NeedsApproval { approvals, .. } = authorization else {
        return ApprovalFacts {
            name: name.to_owned(),
            summary: summary.to_owned(),
            effect: format!("{name} · {summary}"),
            scope: summary.to_owned(),
            reversibility: "not reported".to_owned(),
            reason: authorization.requested(),
            rule_approval: false,
        };
    };
    let actions = approvals
        .iter()
        .map(|request| request.requirement.action.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(" + ");
    let scope = approvals
        .iter()
        .map(|request| {
            format!(
                "{}:{}",
                request.requirement.resource.scheme(),
                request.requirement.resource.value()
            )
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(" + ");
    ApprovalFacts {
        name: name.to_owned(),
        summary: summary.to_owned(),
        effect: if actions.is_empty() {
            name.to_owned()
        } else {
            format!("{name} · {actions}")
        },
        scope,
        reversibility: if approvals.iter().all(|request| request.reversible) {
            "reversible".to_owned()
        } else {
            "irreversible".to_owned()
        },
        reason: authorization.requested(),
        rule_approval: true,
    }
}

/// What authorizing a call decided.
#[cfg(feature = "tui")]
enum Granted {
    Run {
        grants: Vec<arsy_kernel::capability::CapabilityGrant>,
        /// What the operator said when they approved it, if anything.
        note: Option<String>,
    },
    /// The call does not run, and this is what the provider is told.
    Refused(Box<arsy_code::agent::ToolResult>),
    Stopped,
}

/// Turn an authorization into grants, asking the operator when the mode says
/// to ask.
///
/// Separate from running the call: what a call is allowed to do is decided
/// before anything happens, and reading that decision should not mean reading
/// the execution as well.
#[cfg(feature = "tui")]
fn authorize(
    terminal: &mut io::Stdout,
    colour: bool,
    asking: Asking<'_>,
    authorization: arsy_code::agent::Authorization,
    safety: Option<&arsy_kernel::safety::SafetyReviewResult>,
) -> io::Result<Granted> {
    use arsy_code::agent::Authorization;

    let name = asking.name;
    if let Some(review) =
        safety.filter(|review| review.decision == arsy_kernel::safety::SafetyDecision::Deny)
    {
        return Ok(refused(
            name,
            format!("Safe Auto denied this call: {}", review.reasons.join("; ")),
        ));
    }
    let force_approval = safety.is_some_and(|review| {
        review.decision == arsy_kernel::safety::SafetyDecision::RequireApproval
    });
    let requested = match &authorization {
        Authorization::Allowed(grants) if !force_approval => {
            return Ok(Granted::Run {
                grants: grants.clone(),
                note: None,
            })
        }
        Authorization::Allowed(_) => "independent safety review requires approval".into(),
        Authorization::Denied(reason) => return Ok(refused(name, reason.clone())),
        Authorization::NeedsApproval { .. } => authorization.requested(),
    };
    let decision = if force_approval {
        approval::Decision::Ask
    } else {
        approval::decide(asking.approval.get(), name)
    };
    match decision {
        approval::Decision::Approve => Ok(granted(authorization, name, None)),
        approval::Decision::Refuse => Ok(refused(
            name,
            format!(
                "the current approval mode ({}) refuses this call without asking: {}",
                asking.approval.get().label(),
                requested
            ),
        )),
        approval::Decision::Ask => {
            let preview = format_tool_preview(name, asking.arguments);
            let facts = policy_approval_facts(name, asking.summary, &authorization);
            if let Some(grants) = asking.approval.cached(&authorization) {
                return Ok(Granted::Run { grants, note: None });
            }
            match confirm_tool(
                terminal,
                colour,
                &facts,
                preview,
                asking.keys,
                asking.decoder,
                asking.approval,
            )? {
                Answer::Yes { note } => Ok(granted(authorization, name, note)),
                Answer::Rule { note } => {
                    asking.approval.remember(&authorization);
                    writeln!(terminal, "{}", tui::rule_allowed_row(colour, &facts.effect))?;
                    Ok(granted(authorization, name, note))
                }
                Answer::No { note } => Ok(refused(
                    name,
                    note.map_or_else(
                        || "The operator declined to run this call.".to_owned(),
                        |note| format!("The operator declined to run this call. Feedback: {note}"),
                    ),
                )),
                Answer::Stop => Ok(Granted::Stopped),
            }
        }
    }
}

/// Turn an approved authorization into the grants the call runs under.
#[cfg(feature = "tui")]
fn granted(
    authorization: arsy_code::agent::Authorization,
    name: &str,
    note: Option<String>,
) -> Granted {
    match authorization.approve() {
        Ok(grants) => Granted::Run { grants, note },
        Err(error) => refused(
            name,
            format!("the approval could not be turned into a grant: {error}"),
        ),
    }
}

/// The answer a call that will not run sends back to the provider.
#[cfg(feature = "tui")]
fn refused(name: &str, reason: String) -> Granted {
    Granted::Refused(Box::new(arsy_code::agent::ToolResult::refused(
        name, reason,
    )))
}

#[cfg(feature = "tui")]
// Every argument is one the live view needs and none of them group into a
// meaningful type: the terminal, the call, and the keyboard are three unrelated
// things this function happens to hold at once.
#[allow(clippy::too_many_arguments)]
fn dispatch_tool_live(
    terminal: &mut io::Stdout,
    colour: bool,
    runtime: &arsy_code::agent::ToolRuntime,
    name: &str,
    request: &arsy_kernel::operation::OperationRequest,
    grants: &[arsy_kernel::capability::CapabilityGrant],
    started: std::time::Instant,
    summary: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    footer: &str,
) -> io::Result<(arsy_code::agent::ToolResult, bool)> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let (output_sender, output_receiver) = std::sync::mpsc::channel();
    let output_sink: arsy_kernel::operation::OutputSink = std::sync::Arc::new(move |chunk| {
        let _ = output_sender.send(chunk);
    });
    runtime.set_output_sink(Some(output_sink));
    let worker_runtime = runtime.clone();
    let name = name.to_owned();
    let worker_name = name.clone();
    let request = request.clone();
    let operation_id = request.id;
    let worker_request = request.clone();
    let grants = grants.to_vec();
    std::thread::spawn(move || {
        let result = worker_runtime.dispatch(&worker_name, &worker_request, &grants, started);
        let _ = sender.send(result);
    });

    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let mut frame = 0usize;
    let elapsed = std::time::Instant::now();
    let mut cancelled = false;
    let mut expanded = false;
    let mut live_output = String::new();
    let initial_state = tui::RunningToolState {
        name: name.as_str(),
        summary,
        frame: FRAMES[0],
        elapsed_ms: 0,
        live_output: "",
        expanded,
        drafting: false,
    };
    let _last_rendered_lines = 0;
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                state: &tui::RunningToolState<'_>|
     -> io::Result<usize> {
        let card = tui::tool_running_box(tui::terminal_width(), colour, state);
        // The composer is drawn with the card, not instead of it: the input
        // line is where the operator types the next turn while this one runs,
        // and a card that replaced it read as the session being busy at them.
        let mut frame = composer.clear();
        frame.push_str(&card.iter().map(|l| format!("{l}\n")).collect::<String>());
        frame.push_str(&composer.render_turn(
            tui::terminal_width(),
            colour,
            &tui::turn_status(
                colour,
                tui::TurnPhase::Working,
                std::time::Duration::from_millis(
                    u64::try_from(state.elapsed_ms).unwrap_or(u64::MAX),
                ),
                0,
                0,
            ),
            footer,
        ));
        write!(terminal, "{frame}")?;
        terminal.flush()?;
        Ok(card.len())
    };
    let initial_lines = draw(terminal, composer, &initial_state)?;
    let mut last_rendered_lines = initial_lines;
    loop {
        if absorb_live_keys(
            keys,
            decoder,
            terminal,
            LiveCall {
                name: &name,
                cancellable: request.kind.to_string() == "process.exec",
                operation_id,
            },
            &mut last_rendered_lines,
            &mut expanded,
        )? {
            cancelled = true;
        }
        match receiver.recv_timeout(std::time::Duration::from_millis(80)) {
            Ok(result) => {
                runtime.set_output_sink(None);
                // The card is erased; the composer the next frame draws is
                // positioned above where the card used to be.
                if last_rendered_lines > 0 {
                    write!(terminal, "\x1b[{}A\r\x1b[J", last_rendered_lines)?;
                    terminal.flush()?;
                }
                composer.invalidate();
                return Ok((result, cancelled));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                absorb_output(&output_receiver, &mut live_output);
                frame = frame.wrapping_add(1);
                let state = tui::RunningToolState {
                    name: name.as_str(),
                    summary,
                    frame: FRAMES[frame % FRAMES.len()],
                    elapsed_ms: elapsed.elapsed().as_millis(),
                    live_output: &live_output,
                    expanded,
                    drafting: false,
                };
                last_rendered_lines = draw(terminal, composer, &state)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("tool worker disconnected"));
            }
        }
    }
}

#[cfg(feature = "tui")]
fn format_tool_preview(name: &str, arguments: &Value) -> Option<String> {
    match name {
        "apply_patch" | "fs.edit" | "edit" => arguments
            .get("input")
            .or_else(|| arguments.get("patch"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        "fs.write" | "write" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("file");
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            let preview: Vec<String> = content.lines().take(12).map(|l| format!("+{l}")).collect();
            let mut text = format!("--- /dev/null\n+++ {path}\n{}", preview.join("\n"));
            if content.lines().count() > 12 {
                text.push_str(&format!(
                    "\n… ({} lines omitted)",
                    content.lines().count() - 12
                ));
            }
            Some(text)
        }
        "bash" | "shell.execute" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|cmd| format!("$ {cmd}")),
        _ => None,
    }
}

/// Ask the operator whether one tool call may run.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn confirm_tool(
    terminal: &mut io::Stdout,
    colour: bool,
    facts: &ApprovalFacts,
    diff_preview: Option<String>,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    approval: &approval::ApprovalCell,
) -> io::Result<Answer> {
    let mut dialog = if tui::modern_style() {
        tui::AskDialogState::for_approval_details(
            &facts.effect,
            &facts.scope,
            &facts.reversibility,
            &facts.reason,
            diff_preview,
            facts.rule_approval,
        )
    } else {
        tui::AskDialogState::for_approval(&facts.name, &facts.summary, &facts.reason, diff_preview)
    };
    let width = tui::terminal_width();
    let mut rendered_lines = dialog.render(width, colour).lines().count();
    writeln!(terminal, "{}{}", modern_gap(), dialog.render(width, colour))?;
    terminal.flush()?;
    approval.open();
    loop {
        match keys.recv() {
            Ok(byte) => match decoder.feed(byte) {
                Some(key) => {
                    if let Some(result) = dialog.handle_key(key) {
                        write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
                        terminal.flush()?;
                        match result {
                            tui::AskDialogResult::Approve { note } => {
                                return Ok(Answer::Yes { note })
                            }
                            tui::AskDialogResult::ApproveRule { note } if tui::modern_style() => {
                                return Ok(Answer::Rule { note })
                            }
                            tui::AskDialogResult::ApproveRule { note } => {
                                approval.set(approval::ApprovalMode::Auto);
                                return Ok(Answer::Yes { note });
                            }
                            tui::AskDialogResult::Revise { .. } => continue,
                            tui::AskDialogResult::Deny { note } => return Ok(Answer::No { note }),
                            tui::AskDialogResult::CycleMode => continue,
                            tui::AskDialogResult::Cancel => return Ok(Answer::Stop),
                        }
                    } else {
                        let frame = dialog.render(width, colour);
                        write!(terminal, "\x1b[{}A\r\x1b[J{}\n", rendered_lines, frame)?;
                        terminal.flush()?;
                        rendered_lines = frame.lines().count();
                    }
                }
                None => continue,
            },
            Err(_) => return Ok(Answer::Stop),
        }
    }
}

#[cfg(feature = "tui")]
pub(crate) fn confirm_plan(
    terminal: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    preview: &str,
) -> io::Result<tui::AskDialogResult> {
    let mut dialog = tui::AskDialogState::for_plan(preview.to_owned());
    dialog.set_preview_height(tui::terminal_rows().saturating_sub(16));
    let width = tui::terminal_width();
    let mut frame = dialog.render(width, colour);
    let mut rendered_lines = frame.lines().count();
    writeln!(terminal, "{frame}")?;
    terminal.flush()?;
    loop {
        let Ok(byte) = keys.recv() else {
            return Ok(tui::AskDialogResult::Cancel);
        };
        let Some(key) = decoder.feed(byte) else {
            continue;
        };
        if let Some(result) = dialog.handle_key(key) {
            write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
            terminal.flush()?;
            return Ok(result);
        }
        dialog.set_preview_height(tui::terminal_rows().saturating_sub(16));
        frame = dialog.render(width, colour);
        write!(terminal, "\x1b[{}A\r\x1b[J{}\n", rendered_lines, frame)?;
        terminal.flush()?;
        rendered_lines = frame.lines().count();
    }
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn redraw_live_response(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &str,
    status: &str,
    width: usize,
    block: &str,
    prev_lines: usize,
) -> io::Result<usize> {
    let mut frame = composer.clear();
    for _ in 0..prev_lines {
        frame.push_str("\x1b[1A\r\x1b[K");
    }
    let lines = block.lines().count().max(1);
    frame.push_str(block);
    frame.push('\n');
    frame.push_str(&composer.render_turn(width, colour, status, footer));
    write!(terminal, "{frame}")?;
    terminal.flush()?;
    Ok(lines)
}

#[cfg(feature = "tui")]
fn erase_live_response(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    lines: usize,
) -> io::Result<()> {
    let mut frame = composer.clear();
    for _ in 0..lines.max(1) {
        frame.push_str("\x1b[1A\r\x1b[K");
    }
    write!(terminal, "{frame}")?;
    terminal.flush()
}

/// Stream one round of a turn from a configured provider, keeping the composer
/// alive.
///
/// The stream runs on its own thread for the same reason the Codex reader
/// does: the main loop has to keep watching the key stream, which is what lets
/// Esc or Ctrl-C stop a turn and keeps the composer typeable meanwhile. The
/// thread is detached rather than joined, so an interrupt never waits on a
/// stalled socket; dropping the receiver is what stops it, because the next
/// send fails and the stream is dropped with the thread.
#[cfg(feature = "tui")]
/// Call `native_status` for one round, and once more if a stale OAuth
/// access token is why it failed — the interactive-session counterpart to
/// `dispatch_with_refresh`. A session resolves its provider once and keeps
/// it for as long as the operator keeps typing (`resolve_route`'s cache),
/// so a token that expires between turns is never re-checked until this
/// catches it.
#[allow(clippy::too_many_arguments)]
fn native_status_with_refresh(
    resolved: &mut provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
) -> io::Result<Turn> {
    let outcome = native_status(
        resolved,
        config,
        runtime,
        conversation,
        route,
        effort,
        turn,
        round,
        colour,
        footer,
        keys,
        decoder,
        composer,
        approval,
    )?;
    let Some(error) = &outcome.provider_error else {
        return Ok(outcome);
    };
    if !is_stale_oauth_token(error, resolved.source) {
        return Ok(outcome);
    }
    let Ok(refreshed) = provider::resolve(config, Some(&resolved.endpoint.id)) else {
        return Ok(outcome);
    };
    *resolved = refreshed;
    native_status(
        resolved,
        config,
        runtime,
        conversation,
        route,
        effort,
        turn,
        round,
        colour,
        footer,
        keys,
        decoder,
        composer,
        approval,
    )
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn native_status(
    resolved: &provider::Resolved,
    config: &arsy_kernel::config::Config,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
) -> io::Result<Turn> {
    let request = round_request(
        resolved,
        config,
        runtime,
        conversation,
        route,
        effort,
        turn,
        round,
    )?;
    let events = spawn_stream(Arc::clone(&resolved.provider), request);

    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    // Deltas arrive token by token; a row is emitted per line so scrollback
    // reads like the Codex projection rather than one row per token.
    //
    // Thinking and the answer hold separate buffers, and each thinking section
    // is announced once with its own header row, so the verbose stream reads as
    // distinct parts of the turn rather than one grey blur.
    let mut live = Streaming::default();
    let started = std::time::Instant::now();
    // From the clock rather than counted per pass: the loop wakes every
    // `FRAME` while text is being revealed, and the spinner should not spin
    // faster because of it.
    let spinner = || usize::try_from(started.elapsed().as_millis() / 100).unwrap_or(0);
    let mut tick = 0usize;
    // A static `Working…` line cannot tell a slow connect from a hang; the
    // status is rebuilt on every timer pass instead of captured once.
    let status_line = |first_event: bool, tick: usize| {
        tui::turn_status(
            colour,
            if first_event {
                tui::TurnPhase::Answering
            } else {
                tui::TurnPhase::Connecting
            },
            started.elapsed(),
            tick,
            0,
        )
    };
    let mut first_event = false;
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                status: &str| {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, &status_line(false, 0))?;
    loop {
        let typed = drain_keys(keys, decoder, composer, approval, &mut outcome);
        if typed == Typed::Interrupted {
            draw(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                &status_line(first_event, tick),
            )?;
            return finish(terminal, composer, outcome);
        }
        // The status is alive: the spinner advances and the seconds climb even
        // while the provider sends nothing, so a silent turn never reads as a
        // frozen one.
        tick = spinner();
        if typed == Typed::Redraw {
            draw(
                &mut terminal,
                composer,
                None,
                &status_line(first_event, tick),
            )?;
        }
        let paced = live.pace(
            &mut terminal,
            composer,
            colour,
            footer,
            &status_line(first_event, tick),
        )?;
        let wait = if live.has_pending() {
            FRAME
        } else {
            std::time::Duration::from_millis(100)
        };
        match events.recv_timeout(wait) {
            Ok(Ok(Streamed::Thinking(text))) => {
                let status = status_line(first_event, tick);
                live.reason(&mut terminal, composer, colour, footer, &status, &text)?;
                first_event = true;
            }
            Ok(Ok(Streamed::Text(text))) => {
                outcome.response.push_str(&text);
                let status = status_line(first_event, tick);
                live.answer(&mut terminal, composer, colour, footer, &status, &text)?;
                first_event = true;
            }
            Ok(Ok(Streamed::Usage {
                input_tokens,
                output_tokens,
            })) => {
                outcome.usage =
                    json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
            Ok(Ok(Streamed::Tool {
                id,
                name,
                arguments,
            })) => {
                outcome.calls.push((id, name, arguments));
                first_event = true;
            }
            Ok(Err(failure)) => {
                outcome.failure = Some(failure.to_string());
                outcome.provider_error = Some(failure);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if decoder.flush_escape() == Some(tui::Key::Interrupt) {
                    outcome.interrupted = true;
                    live.abandon(&mut terminal, composer)?;
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::interrupted_row(colour)),
                        &status_line(first_event, tick),
                    )?;
                    return finish(terminal, composer, outcome);
                }
                // Repaint the live status on every idle pass, unless a
                // reveal has just drawn it.
                if !paced {
                    draw(
                        &mut terminal,
                        composer,
                        None,
                        &status_line(first_event, tick),
                    )?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    live.close(
        &mut terminal,
        composer,
        colour,
        footer,
        &status_line(first_event, tick),
    )?;
    finish(terminal, composer, outcome)
}

/// One streamed fact from a provider, as the terminal needs it.
#[cfg(feature = "tui")]
enum Streamed {
    Text(String),
    Thinking(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A complete tool call. The host runs it after the stream ends, so a turn
    /// is never edited from under a model that is still writing.
    Tool {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Tear the composer down so the next thing printed starts on its own line.
#[cfg(feature = "tui")]
fn finish(mut terminal: io::Stdout, composer: &mut tui::Composer, turn: Turn) -> io::Result<Turn> {
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    Ok(turn)
}

#[cfg(feature = "tui")]
/// Run the task through the logged-in Codex CLI and project its JSONL event
/// stream as ARSY rows, so the terminal shows one interface, not two.
#[allow(clippy::too_many_arguments)]
fn external_status(
    workspace: &Path,
    task: &str,
    route: &tui::ModelRoute,
    approval: &approval::ApprovalCell,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let mode = approval.get();
    let mut command = std::process::Command::new("codex");
    command.args([
        "exec",
        "--json",
        "--ephemeral",
        "--sandbox",
        if matches!(
            mode,
            approval::ApprovalMode::AcceptEdits
                | approval::ApprovalMode::Auto
                | approval::ApprovalMode::BypassPermissions
        ) {
            "workspace-write"
        } else {
            "read-only"
        },
        "--cd",
    ]);
    command.arg(workspace);
    if route.model != "default" {
        command.args(["--model", &route.model]);
    }
    command.arg("-");
    command.current_dir(workspace);
    let task = if mode == approval::ApprovalMode::Plan {
        format!(
            "{}\n\n{}",
            arsy_code::agent::instructions::PLAN_MODE_INSTRUCTIONS,
            task
        )
    } else {
        task.to_owned()
    };
    drive_provider(
        command, &task, route, approval, colour, footer, keys, decoder, composer, redactor,
    )
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_provider(
    command: std::process::Command,
    task: &str,
    route: &tui::ModelRoute,
    approval: &approval::ApprovalCell,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let (mut child, errors, input, events) = spawn_provider(command, task)?;
    let started = std::time::Instant::now();
    let mut cancelling = None;
    let mut stream_closed = false;
    let mut finished = None;
    let mut last_event = std::time::Instant::now();
    let mut stopped_early = false;
    let mut last_row = None;
    let mut last_key = std::time::Instant::now();
    let mut exited = None;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut outcome = Turn::default();
    let mut successful_git_commands = std::collections::HashSet::new();
    let mut terminal = io::stdout();
    composer.set_height(tui::terminal_rows());
    let painter = Painter {
        colour,
        footer,
        width: std::cell::Cell::new(tui::terminal_width()),
        started,
    };
    painter.row(&mut terminal, composer, None, false, 0, 0)?;
    let mut refreshed = std::time::Instant::now();
    loop {
        let tick = (started.elapsed().as_millis() / 100) as usize;
        if let Ok(result) = input.try_recv() {
            if !outcome.interrupted {
                result?;
            }
        }
        match lifecycle(
            &mut child,
            Clocks {
                started,
                status: &mut status,
                exited: &mut exited,
                finished: &mut finished,
                last_event,
                cancelling,
                stopped_early: &mut stopped_early,
            },
        )? {
            Pass::Stop => break,
            Pass::Go => {}
        }
        let typed = provider_keys(
            Keyboard {
                keys,
                decoder,
                composer,
                approval,
            },
            &mut terminal,
            &painter,
            &mut child,
            Turning {
                outcome: &mut outcome,
                cancelling: &mut cancelling,
                last_key: &mut last_key,
            },
            colour,
            tick,
        )?;
        if last_key.elapsed() >= std::time::Duration::from_millis(40)
            && decoder.flush_escape() == Some(tui::Key::Interrupt)
            && stop_turn(&mut outcome, &mut child, &mut cancelling)
        {
            painter.row(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                true,
                0,
                tick,
            )?;
        }
        let resize_tick = remeasure(&painter, composer, &mut refreshed);
        if typed || resize_tick {
            painter.row(
                &mut terminal,
                composer,
                None,
                cancelling.is_some(),
                outcome.queued.len(),
                tick,
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(_) if outcome.interrupted => {}
            Ok(line) => {
                last_event = std::time::Instant::now();
                show_event(
                    &line?,
                    redactor,
                    &mut child,
                    &painter,
                    &mut terminal,
                    composer,
                    Streamlined {
                        outcome: &mut outcome,
                        finished: &mut finished,
                        stopped_early: &mut stopped_early,
                        seen_git: &mut successful_git_commands,
                        last_row: &mut last_row,
                    },
                    colour,
                    tick,
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Repaint so the spinner and clock stay alive while the
                // provider is quiet, not just when an event or a key arrives.
                painter.row(
                    &mut terminal,
                    composer,
                    None,
                    cancelling.is_some(),
                    outcome.queued.len(),
                    tick,
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => stream_closed = true,
        }
        if stream_closed {
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    if !outcome.interrupted && finished.is_none() {
        return Err(silent_provider(&errors, redactor)?);
    }
    if !outcome.interrupted {
        outcome.failure = verdict(&mut child, route, status, stopped_early, &outcome)?;
    }
    Ok(outcome)
}

/// What one interactive turn left behind, whichever route ran it.
#[cfg(feature = "tui")]
#[derive(Default)]
pub(crate) struct Turn {
    /// `None` when the turn succeeded; otherwise why it did not.
    pub(crate) failure: Option<String>,
    /// The typed error `failure` was rendered from, when this turn's
    /// failure came from a provider stream at all — `None` for the
    /// round-limit and external-CLI failure paths, neither of which is a
    /// [`ProviderError`]. Kept separately so a caller can tell a stale
    /// OAuth token apart from anything else without parsing `failure`'s
    /// display text.
    pub(crate) provider_error: Option<arsy_kernel::provider::ProviderError>,
    /// The full text of the model's answer, kept so follow-up turns in the
    /// same session know what the model said.
    pub(crate) response: String,
    /// Extra facts to record on a completed turn, such as token usage.
    pub(crate) usage: Value,
    pub(crate) interrupted: bool,
    pub(crate) provider_failed: bool,
    /// A line submitted while this turn was still running.
    pub(crate) queued: std::collections::VecDeque<String>,
    pub(crate) quit: bool,
    /// Tool calls the model made and the host has not run yet: id, name, and
    /// arguments. Only complete calls land here, so a truncated stream cannot
    /// leave a half-parsed call to execute.
    pub(crate) calls: Vec<(String, String, Value)>,
    pub(crate) changed_files: std::collections::BTreeSet<String>,
    pub(crate) rules_granted: usize,
}
#[cfg(feature = "tui")]
/// Record and report a turn the provider did not complete.
///
/// The code and the reason follow the route, because "the CLI failed" is not
/// something to tell an operator whose turn went straight to an endpoint, and
/// the recorded reason is what a later audit reads.
fn fail_turn(
    service: &AgentService,
    actor: Principal,
    turn: arsy_kernel::domain::TurnId,
    session: SessionId,
    message: String,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let (code, reason, remediation) = if message.contains("asked for tools")
        || message.contains("repeated the same failing tool call")
    {
        // The harness stopped the turn, not the provider: misreporting a
        // local budget as ARSY-PRV-1000 sends an operator chasing endpoint
        // and credential problems that do not exist.
        (
            ARSY_TRN_1000,
            "turn",
            "raise `execution.max_tool_rounds`, or continue with a narrower task",
        )
    } else {
        (
            ARSY_PRV_1000,
            "provider",
            "check the provider endpoint, credential, and model in `arsy config explain`",
        )
    };
    let diagnostic = Diagnostic::error(code, message, remediation);
    service
        .fail_turn(actor, turn, reason, diagnostic.message.clone())
        .map_err(storage_failed)?;
    emitter.diagnostic(&diagnostic);
    turn_record(
        emitter,
        json!({
            "session": session.to_string(),
            "turn": turn.to_string(),
            "status": "failed",
        }),
    );
    Ok(diagnostic.exit_code())
}

/// Emit the machine record for one interactive turn.
///
/// The record is the audit trail machines read, so `--output json` and `ci`
/// keep it. Printing it after every reply in the interactive terminal only
/// buries the reply, and the same evidence is already durable in the session
/// store, reachable with `arsy resume`.
#[cfg(feature = "tui")]
fn turn_record(emitter: &mut Emitter, payload: Value) {
    if emitter.output != Output::Human {
        emitter.result(payload);
    }
}

/// Run one round's tool calls and answer each to the model.
///
/// Extracted from `native_turn`'s round loop so the loop stays a loop over
/// rounds; every call is still shown and answered, or reported as skipped.
/// Returns the results, whether every call was a skipped duplicate, and the
/// files those calls changed.
#[allow(clippy::too_many_arguments)]
fn run_round_calls(
    calls: &[(String, String, Value)],
    runtime: &arsy_code::agent::ToolRuntime,
    intent_digest: arsy_kernel::domain::StateVersion,
    terminal: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    approval: &approval::ApprovalCell,
    hooks: Option<&arsy_code::hook::HookEngine>,
    composer: &mut tui::Composer,
    footer: &str,
    transcript: &mut tui::Transcript,
    completed_calls: &mut std::collections::HashMap<String, String>,
    interrupted: &mut bool,
) -> io::Result<(Vec<ModelContent>, bool, Vec<String>)> {
    let mut results = Vec::with_capacity(calls.len());
    let mut all_repeated = true;
    let mut changed = Vec::new();
    for (id, name, arguments) in calls {
        let summary = runtime.summarize(name, arguments);
        let fingerprint = tool_call_fingerprint(name, arguments);
        let cached = completed_calls.get(&fingerprint).cloned();
        let repeated = cached.is_some();
        let result = match (*interrupted, cached) {
            // Once the turn is stopped the remaining calls are still answered,
            // because a call the provider sent needs a result; they are simply
            // answered without running anything.
            (true, _) => {
                all_repeated = false;
                CallResult {
                    output: "The operator declined to run this call.".to_owned(),
                    is_error: true,
                    metadata: Value::Null,
                    changed_files: Vec::new(),
                    duration: std::time::Duration::ZERO,
                }
            }
            (false, Some(previous)) => CallResult {
                output: format!(
                    "This exact tool call already completed successfully; skipped duplicate.\n\
                     {previous}"
                ),
                is_error: false,
                metadata: Value::Null,
                changed_files: Vec::new(),
                duration: std::time::Duration::ZERO,
            },
            (false, None) => {
                all_repeated = false;
                run_call(
                    runtime,
                    intent_digest,
                    terminal,
                    colour,
                    &summary,
                    Call {
                        name,
                        arguments,
                        fingerprint,
                    },
                    Answering {
                        keys,
                        decoder,
                        approval,
                        completed: completed_calls,
                        interrupted,
                        hooks,
                        composer,
                        footer,
                    },
                )?
            }
        };
        changed.extend(result.changed_files.iter().cloned());
        let todo = (name.starts_with("todo.") || name.starts_with("todo_"))
            .then(|| tui::todo_block(&result.metadata, colour))
            .flatten();
        if !repeated {
            if todo.is_some() {
                transcript.push_todos(&result.metadata);
            } else {
                transcript.push_tool(
                    name,
                    &summary,
                    &result.output,
                    !result.is_error,
                    result.duration,
                );
            }
        }
        let card = if repeated {
            tui::tool_result_row(colour, name, true, "duplicate skipped")
        } else if let Some(todo) = todo {
            todo
        } else {
            tui::tool_card(
                tui::terminal_width(),
                colour,
                name,
                &summary,
                &result.output,
                !result.is_error,
                result.duration,
            )
        };
        writeln!(
            terminal,
            "{}{}{}",
            tui::DISABLE_AUTOWRAP,
            card,
            tui::ENABLE_AUTOWRAP
        )?;
        terminal.flush()?;
        results.push(ModelContent::ToolResult {
            id: id.clone(),
            content: result.output,
            is_error: result.is_error,
        });
    }
    Ok((results, all_repeated, changed))
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    #[test]
    fn a_small_backlog_reveals_one_whole_word() {
        assert_eq!(reveal_len("hello wor"), "hello ".len());
        assert_eq!(reveal_len("hello "), "hello ".len());
        assert_eq!(reveal_len("\n\nhello there"), "\n\nhello ".len());
    }

    #[test]
    fn a_partial_word_waits_for_the_rest_of_itself() {
        assert_eq!(reveal_len("hel"), 0);
        assert_eq!(reveal_len("   "), 0);
        assert_eq!(reveal_len(""), 0);
    }

    #[test]
    fn a_large_backlog_reveals_a_tenth_of_itself() {
        let backlog = "word ".repeat(100);
        assert_eq!(reveal_len(&backlog), "word ".len() * 10);
    }

    #[test]
    fn text_without_spaces_reveals_a_character_at_a_time() {
        assert_eq!(reveal_len("こんにちは世界"), "こ".len());
        assert_eq!(reveal_len("hello世界"), "hello".len());
        let backlog = "世".repeat(100);
        assert_eq!(reveal_len(&backlog), "世".len() * 10);
    }

    #[test]
    fn text_that_never_sends_a_boundary_is_revealed_after_a_stall() {
        let mut live = Streaming::default();
        let mut composer = tui::Composer::default();
        let mut screen: Vec<u8> = Vec::new();
        // Thai is written without spaces and is not treated as unspaced.
        live.answer(&mut screen, &mut composer, false, "", "status", "สวัสดี")
            .unwrap();
        assert!(!live
            .pace(&mut screen, &mut composer, false, "", "status")
            .unwrap());
        live.waiting_since = Some(std::time::Instant::now() - STALL);
        assert!(live
            .pace(&mut screen, &mut composer, false, "", "status")
            .unwrap());
        assert!(!live.has_pending(), "the stalled text is shown");
    }

    #[test]
    fn a_long_answer_splits_at_its_last_paragraph() {
        let (settled, rest, paragraph) = settle_split("one\n\ntwo\n\nthree").unwrap();
        assert_eq!(settled, "one\n\ntwo\n\n");
        assert_eq!(rest, "three");
        assert!(paragraph);
    }

    #[test]
    fn a_split_inside_a_fence_closes_and_reopens_it() {
        let (settled, rest, paragraph) =
            settle_split("```rust\nlet a = 1;\n\nlet b = 2;\nlet c").unwrap();
        assert_eq!(settled, "```rust\nlet a = 1;\n\nlet b = 2;\n```\n");
        assert_eq!(rest, "```rust\nlet c");
        assert!(!paragraph, "a blank line inside code is not a paragraph");
    }

    #[test]
    fn a_single_line_has_nowhere_to_split() {
        assert!(settle_split("one long line").is_none());
    }

    #[test]
    fn a_long_answer_never_keeps_more_live_rows_than_the_screen_holds() {
        let mut live = Streaming::default();
        let mut composer = tui::Composer::default();
        let mut screen: Vec<u8> = Vec::new();
        let answer: String = (0..80).map(|n| format!("paragraph {n}\n\n")).collect();
        live.answer(&mut screen, &mut composer, false, "", "status", &answer)
            .unwrap();
        while live.has_pending() {
            live.last_frame = None;
            live.pace(&mut screen, &mut composer, false, "", "status")
                .unwrap();
            assert!(
                live.live_lines <= tui::terminal_rows(),
                "{} live rows cannot all be erased",
                live.live_lines
            );
        }
        let before_close = screen.len();
        live.close(&mut screen, &mut composer, false, "", "status")
            .unwrap();

        // What `close` settles is the tail of an answer whose head is already
        // in scrollback, so it carries no second marker.
        let tail = String::from_utf8(screen[before_close..].to_vec()).unwrap();
        assert!(live.continued, "the head settled while it streamed");
        assert!(!tail.contains('✦'), "one marker per answer:\n{tail}");
        assert!(tail.contains("paragraph 79"), "the last words are drawn");
    }
}
