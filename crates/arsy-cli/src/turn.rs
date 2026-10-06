//! One interactive turn, whichever route runs it: the native provider loop
//! with its tool calls and approvals, the external Codex CLI projection, and
//! the recording of what the turn left behind.

use crate::run::{charge_turn, is_stale_oauth_token, merge, prepare_task, refusal, task_budget};
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
    budget: arsy_kernel::orchestration::Budget,
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
            budget,
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
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
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

/// Whether a failed turn's work stays in the conversation.
///
/// A rejected or undecodable request may have been caused by the history it
/// carried; keeping that history would send it again with every later turn
/// and fail each one the same way, so such a turn is rewound. Anything else —
/// an outage, a lapsed login, a loop or round limit — says nothing against
/// the history, and the work done before it is real.
#[cfg(feature = "tui")]
fn keeps_failed_work(error: Option<&arsy_kernel::provider::ProviderError>) -> bool {
    !matches!(
        error,
        Some(
            arsy_kernel::provider::ProviderError::InvalidRequest(_)
                | arsy_kernel::provider::ProviderError::Decode(_)
        )
    )
}

/// Leave a stopped turn's history where the next turn can see it.
///
/// An interrupted turn is kept, not rewound: whatever ran before the stop has
/// already changed the workspace, and a next turn that cannot see it redoes or
/// undoes it. A failed turn is kept the same way unless [`keeps_failed_work`]
/// says the history may be what failed.
#[cfg(feature = "tui")]
fn settle_stopped_turn(conversation: &mut Vec<ModelMessage>, base: usize, turn: &Turn) {
    if turn.interrupted {
        close_stopped_turn(
            conversation,
            &turn.response,
            "[The operator interrupted this turn here. Tool calls above that ran have \
             already taken effect.]",
        );
    } else if let Some(failure) = &turn.failure {
        if keeps_failed_work(turn.provider_error.as_ref()) {
            close_stopped_turn(
                conversation,
                &turn.response,
                &format!(
                    "[This turn stopped here: {failure}. Tool calls above that ran have \
                     already taken effect.]"
                ),
            );
        } else {
            conversation.truncate(base);
        }
    }
}

/// End a stopped turn's history with what the model had said and why it
/// stopped, so the next turn starts from where this one actually left off.
///
/// Every call in the kept history already has its result — a stopped round
/// answers the calls it did not run — so only the closing note is added, as
/// the model's own last word.
#[cfg(feature = "tui")]
fn close_stopped_turn(conversation: &mut Vec<ModelMessage>, partial: &str, note: &str) {
    let mut text = partial.trim_end().to_owned();
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(note);
    match conversation.last_mut() {
        Some(last) if last.role == ModelRole::Assistant => {
            last.content.push(ModelContent::Text { text });
        }
        _ => conversation.push(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::Text { text }],
        }),
    }
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
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
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
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    node: TaskId,
    route: &tui::ModelRoute,
    native: Option<&provider::Resolved>,
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
        }),
    );
    let exchange = transcript::persistable(&conversation[base..]);
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
        .record_transcript(actor.clone(), turn_id, &exchange)
        .map_err(storage_failed)?;
    service
        .complete_turn(actor.clone(), turn_id, &outcome)
        .map_err(storage_failed)?;
    graph.complete(node, outcome).map_err(graph_failed)?;
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
    actor: &Principal,
    turn_id: arsy_kernel::domain::TurnId,
    session: SessionId,
    node: TaskId,
    route: &tui::ModelRoute,
    native: Option<&provider::Resolved>,
    base: usize,
    conversation: &mut Vec<ModelMessage>,
    turn: &Turn,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    settle_stopped_turn(conversation, base, turn);
    if turn.interrupted {
        return persist_interrupted_turn(
            service, graph, actor, turn_id, session, node, route, emitter,
        );
    }
    if let Some(failure) = &turn.failure {
        return persist_failed_turn(
            service, graph, actor, turn_id, session, node, failure, emitter,
        );
    }
    persist_completed_turn(
        service,
        graph,
        actor,
        turn_id,
        session,
        node,
        route,
        native,
        base,
        conversation,
        turn,
        emitter,
    )
}

#[cfg(feature = "tui")]
fn selected_turn_budget(
    native: Option<&mut provider::Resolved>,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
) -> Result<arsy_kernel::orchestration::Budget, Diagnostic> {
    let Some(resolved) = native else {
        return Ok(crate::run::EXTERNAL_TASK_BUDGET);
    };
    let model = tui::variant_for(&resolved.endpoint.models, &route.model, effort);
    provider::ensure_context_window(resolved, &model).map_err(|reason| {
        Diagnostic::error(
            ARSY_PRV_1000,
            reason,
            "use provider model metadata or a verified per-model limit",
        )
    })?;
    task_budget(&resolved.endpoint, &model).map_err(|reason| {
        Diagnostic::error(
            ARSY_PRV_1000,
            reason,
            "configure the selected model's context window",
        )
    })
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
    footer: &Footer<'_>,
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
    let config = load_session_config(&root, &working, invocation)?;
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
    let budget = selected_turn_budget(native.as_deref_mut(), route, effort)?;
    let RecordedTurn {
        service,
        mut graph,
        actor,
        admission,
        session,
        task: node,
        store,
    } = record_turn(invocation, session_id, task.clone(), budget, emitter)?;
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
    approval.set_configured_commands(config.allow_commands());
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
            )
            .inspect_err(|_| conversation.truncate(base))?
            .with_execution_mode(approval.get().execution_mode());
            approval.carry_directories(&runtime);
            show_mcp_panel(emitter, transcript, colour);
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
        // Only a Codex route may fall back to the Codex CLI. Any other route
        // that did not resolve fails here: silently answering it through a
        // different agent would run it outside this harness's approvals.
        None if route.provider != "codex" => Err(io::Error::other(unavailable_provider(
            &config,
            &route.provider,
        ))),
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
            // A tool call already pushed without its result would be refused
            // by every provider on the next turn, so the failure rewinds too.
            conversation.truncate(base);
            let reason = format!("could not run {route}: {error}");
            graph
                .fail(node, json!({"message": reason.clone()}))
                .map_err(graph_failed)?;
            fail_turn(&service, actor, admission.turn, session, reason, emitter)?;
            return Ok(Turn::default());
        }
    };
    turn.rules_granted = recorded_rules.len();
    for detail in &turn.compactions {
        let mut detail = detail.clone();
        merge(&mut detail, json!({"turn_id": admission.turn}));
        emitter.trace("context.compacted", detail.clone());
        service
            .record_compaction(actor.clone(), &detail)
            .map_err(storage_failed)?;
    }
    turn.failure = turn.failure.or((conversation.len() < base)
        .then(|| "the conversation changed while the turn was running".to_owned()));
    if !turn.recorded && !turn.interrupted && turn.failure.is_none() {
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
        &actor,
        admission.turn,
        session,
        node,
        route,
        native.as_deref(),
        base,
        conversation,
        &turn,
        emitter,
    )?;
    let title = title_after_turn(
        &root,
        session,
        &config,
        &task,
        &turn,
        native.as_deref(),
        route,
        effort,
        admission.turn,
    );
    record_turn_end(
        &turn, transcript, &store, session, session_id, title, colour,
    )?;
    Ok(turn)
}

/// Title a session after the turn that first answered in it, and say what it
/// is called now so the footer can lead with it.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn title_after_turn(
    root: &Path,
    session: SessionId,
    config: &arsy_kernel::config::Config,
    task: &str,
    turn: &Turn,
    native: Option<&provider::Resolved>,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn_id: arsy_kernel::domain::TurnId,
) -> Option<String> {
    if turn.interrupted || turn.failure.is_some() || turn.response.trim().is_empty() {
        return None;
    }
    let titles = open_store(root).ok()?;
    let model = native.map(|resolved| crate::session_title::TitleModel {
        provider: Arc::clone(&resolved.provider),
        // The model that answered the turn, not the picker's base name: a
        // provider listing one model per effort serves only the variants.
        model: arsy_kernel::provider::ModelKey {
            provider: route.provider.clone(),
            model: routed_model(resolved, route, effort).0,
        },
        turn: turn_id.to_string(),
    });
    crate::session_title::title_new_session(
        Arc::clone(&titles),
        session,
        config.session_title(),
        task,
        &turn.response,
        model,
    );
    titles.session_title(session).ok().flatten()
}

/// Record how the turn ended — the `Interrupted` row, the failure, or the
/// session footer — so a repaint draws it again. The footer is drawn here;
/// the other two were drawn when they happened.
#[cfg(feature = "tui")]
fn record_turn_end(
    turn: &Turn,
    transcript: &mut tui::Transcript,
    store: &Arc<dyn EventStore>,
    session: SessionId,
    session_id: SessionId,
    title: Option<String>,
    colour: bool,
) -> Result<(), Diagnostic> {
    if turn.interrupted {
        transcript.push_interrupted();
        return Ok(());
    }
    if let Some(failure) = &turn.failure {
        transcript.push_failure(failure);
        return Ok(());
    }
    if turn.response.trim().is_empty() {
        return Ok(());
    }
    let footer = tui::SessionFooter {
        session: session_id.to_string(),
        title,
        changed_files: turn.changed_files.len(),
        rules_granted: turn.rules_granted,
        events: store.current_version(session).map_err(storage_failed)?.0,
    };
    let _ = writeln!(
        io::stdout(),
        "{}{}",
        modern_gap(),
        footer.render(tui::terminal_width(), colour)
    );
    transcript.push_footer(footer);
    Ok(())
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
fn compact_round_view(
    conversation: &[ModelMessage],
    history: &arsy_code::agent::budget::History,
    budget: u32,
    model: &str,
    colour: bool,
    transcript: &mut tui::Transcript,
    composer: &mut tui::Composer,
    reported_trim: &mut (usize, usize),
) -> io::Result<(Vec<ModelMessage>, Option<Value>)> {
    let mut progress = CompactionProgress::new(
        arsy_code::agent::budget::conversation_tokens(conversation),
        Some(budget),
    );
    let (view, trimmed) = arsy_code::agent::budget::view_reporting(
        conversation,
        budget,
        Some(history),
        &mut |stage| {
            let _ = progress.show(
                &mut io::stdout(),
                Some(&mut *composer),
                colour,
                compaction_step(stage),
            );
        },
    );
    if trimmed.after > budget {
        return Err(io::Error::other(format!(
            "model `{model}` input still exceeds its context budget after compaction"
        )));
    }
    let new_trim = (trimmed.elided, trimmed.summarized) != *reported_trim;
    *reported_trim = (trimmed.elided, trimmed.summarized);
    let shown = if new_trim {
        trimmed
    } else {
        arsy_code::agent::budget::Trimmed::default()
    };
    let detail = report_trim(
        &mut io::stdout(),
        colour,
        &shown,
        false,
        transcript,
        composer,
        &mut progress,
    )?;
    Ok((view, detail))
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
    footer: &Footer<'_>,
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
    // Repeating a call that just *failed* is a loop the operator cannot see
    // past the tool cards; the guard warns the model, then stops it.
    let mut stuck = crate::loop_guard::LoopGuard::default();
    // Rounds of nothing but repeated successful reads before the turn ends.
    const FAILURE_LOOP_LIMIT: usize = 3;
    // Consecutive rounds made only of repeated calls. A repeated read is the
    // model re-checking what it saw, which is still exploring, so it is
    // answered from the memo and the turn goes on; the same bound as a
    // failure loop stops a model that only ever repeats itself.
    let mut repeated_rounds = 0usize;
    let mut reported_trim = (0, 0);
    // What each new compaction recorded, for the turn to append once it has
    // the session open.
    let mut compactions = Vec::new();
    // A clone shares the runtime's handles, so audits it records still reach
    // the caller's runtime; only the execution mode is this turn's own.
    let mut runtime = runtime.clone();
    // The effort is read back each round, like the mode, so Ctrl+T or
    // `/effort` while the turn runs reaches its next request.
    approval.set_effort(effort);
    for round in 0..max_rounds {
        // Shift+Tab can change the mode mid-turn. The tool list and the
        // runtime's own refusal follow it from the next request, rather than
        // staying on the mode the turn started in while `decide` reads the
        // new one.
        let mode = approval.get().execution_mode();
        if runtime.execution_mode() != mode {
            runtime = runtime.with_execution_mode(mode);
        }
        steer_into(conversation, composer, transcript, colour)?;
        let model = tui::variant_for(&resolved.endpoint.models, &route.model, approval.effort());
        provider::ensure_context_window(resolved, &model).map_err(io::Error::other)?;
        // Before the request, not after: a transcript that has outgrown the
        // window fails at the provider, and the operator is told what was
        // elided rather than watching the turn shrink invisibly. The request
        // is a trimmed view; `conversation` stays whole, because the turn's
        // own messages are sliced out of it by index once it ends.
        //
        // A compaction draws a live row while it runs, so the turn never
        // goes quiet between the prompt and the model's first word.
        let sizing = round_request(
            resolved,
            config,
            &runtime,
            conversation,
            route,
            approval.effort(),
            turn,
            round,
        )?;
        let budget =
            crate::run::request_budget(&resolved.endpoint, &sizing).map_err(io::Error::other)?;
        let (view, detail) = compact_round_view(
            conversation,
            history,
            budget,
            &sizing.model.model,
            colour,
            transcript,
            composer,
            &mut reported_trim,
        )?;
        compactions.extend(detail);
        let mut outcome = native_status_with_refresh(
            resolved,
            config,
            &runtime,
            &view,
            route,
            approval.effort(),
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
        record_round(transcript, &mut outcome);
        if outcome.calls.is_empty() || outcome.interrupted || outcome.failure.is_some() {
            outcome.changed_files = changed_files;
            outcome.compactions = compactions;
            return Ok(outcome);
        }
        // The calls are history now, whatever the operator decides about them:
        // a provider that sent a call and never sees its result rejects the
        // next request.
        let calls = std::mem::take(&mut outcome.calls);
        // The model's reasoning goes first, ahead of the calls it led to, so
        // the next round continues it instead of starting its plan again.
        let mut content: Vec<ModelContent> = std::mem::take(&mut outcome.reasoning)
            .into_iter()
            .map(|state| ModelContent::Reasoning { state })
            .collect();
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
            &runtime,
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
        // Judged before the results join the conversation, so a redirect can
        // ride on them. A stopped turn's declined calls are not a loop.
        let mut results = results;
        let verdict = if outcome.interrupted {
            crate::loop_guard::Verdict::Continue
        } else {
            stuck.observe(&calls, &results)
        };
        if let crate::loop_guard::Verdict::Redirect(note) = &verdict {
            crate::loop_guard::attach_note(&mut results, note);
        }
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
        let ends;
        (repeated_rounds, ends) = repeated_round(
            &runtime,
            &calls,
            all_repeated,
            repeated_rounds,
            FAILURE_LOOP_LIMIT,
        );
        if ends {
            outcome.response =
                "The requested operation already completed; a repeated tool call was skipped."
                    .to_owned();
            outcome.changed_files = changed_files;
            outcome.compactions = compactions;
            return Ok(outcome);
        }
        // The response of a round that called tools belongs to the history
        // above, not to the answer this turn returns.
        outcome.response.clear();
        if outcome.interrupted {
            outcome.changed_files = changed_files;
            outcome.compactions = compactions;
            return Ok(outcome);
        }
        // A loop the model was already warned about: stop the turn with a
        // message that names the loop rather than the provider.
        if let crate::loop_guard::Verdict::Stop(reason) = verdict {
            outcome.failure = Some(format!("{route}: {reason}"));
            outcome.changed_files = changed_files;
            outcome.compactions = compactions;
            return Ok(outcome);
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
        compactions,
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
    let (width, rows) = tui::terminal_dimensions();
    painter.width.set(width);
    composer.set_height(rows);
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

/// Count a round made only of repeated calls, and say whether the run of
/// them ends the turn.
///
/// A repeated effect ends it at once: running it again is what the memo
/// exists to prevent. A repeated read is the model re-checking what it saw,
/// so the turn goes on until `limit` such rounds in a row. Any round that ran
/// something new resets the count.
#[cfg(feature = "tui")]
fn repeated_round(
    runtime: &arsy_code::agent::ToolRuntime,
    calls: &[(String, String, Value)],
    all_repeated: bool,
    rounds: usize,
    limit: usize,
) -> (usize, bool) {
    if !all_repeated || calls.is_empty() {
        return (0, false);
    }
    let rounds = rounds + 1;
    let reads_only = calls
        .iter()
        .all(|(_, name, arguments)| runtime.is_observational(name, arguments));
    (rounds, rounds >= limit || !reads_only)
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
/// `e` on an empty line or Ctrl-O toggles how much of the output is shown,
/// Shift+Tab and Ctrl+T
/// change the mode and effort at once, and everything else is typing into
/// the composer drawn under the card: a line sent is held for the turn to
/// queue, the same as one sent while the model streams.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn absorb_live_keys(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    approval: &approval::ApprovalCell,
    terminal: &mut io::Stdout,
    call: LiveCall<'_>,
    drawn_rows: &mut usize,
    expanded: &mut bool,
) -> io::Result<bool> {
    let mut cancelled = false;
    for key in keys.try_iter().filter_map(|byte| decoder.feed(byte)) {
        if key == tui::Key::Interrupt {
            if call.cancellable {
                arsy_code::process::cancel(call.operation_id);
                if *drawn_rows > 0 {
                    write!(terminal, "{}", erase_card(composer, *drawn_rows))?;
                    *drawn_rows = 0;
                }
                // Stopping the command stops the turn, and the lines waiting
                // on it go with it, as Esc takes them while the model streams.
                composer.take_held();
                write!(terminal, "\r\x1b[K  ✦ Cancelling {}…\n", call.name)?;
                terminal.flush()?;
                cancelled = true;
            }
            continue;
        }
        // `e` on an empty line still expands the running command's output,
        // as it always has; once a draft has begun it is a letter like any
        // other, and Ctrl-O expands instead.
        if matches!(key, tui::Key::Char('e' | 'E')) && composer.is_empty() {
            *expanded = !*expanded;
            continue;
        }
        if key == tui::Key::Tab {
            queue_draft(composer);
            continue;
        }
        match composer.press(key) {
            action if live_control(&action, approval) => {}
            tui::Action::Expand => *expanded = !*expanded,
            tui::Action::Submit(line) if !line.trim().is_empty() => keep_sent(composer, line, true),
            _ => {}
        }
    }
    Ok(cancelled)
}

/// Keep a line sent while a turn runs: steering it into the turn (Enter) or
/// queueing it for after (Tab). Bounded, so a held key cannot grow the queue
/// without limit; past the bound the draft is handed back.
#[cfg(feature = "tui")]
fn keep_sent(composer: &mut tui::Composer, line: String, steer: bool) {
    if composer.held_len() >= 16 {
        composer.restore(line);
    } else if steer {
        composer.steer(line);
    } else {
        composer.hold(line);
    }
}

/// Tab while a turn runs: queue the draft for after the turn instead of
/// steering it in, as the Codex CLI does. Taken through the same submit Enter
/// uses, so pastes and history behave alike. Answers whether a line was taken.
#[cfg(feature = "tui")]
fn queue_draft(composer: &mut tui::Composer) -> bool {
    if composer.is_empty() {
        return false;
    }
    match composer.press(tui::Key::Enter) {
        tui::Action::Submit(line) if !line.trim().is_empty() => {
            keep_sent(composer, line, false);
            true
        }
        _ => false,
    }
}

/// Put the lines the operator steered in while the turn ran into the
/// conversation, beside the tool results the model is about to read.
///
/// Only where the conversation ends in those results — the start of a later
/// round — so a model request always follows and answers them; a turn that
/// ends first leaves them held, and they run as follow-ups instead. Each is
/// drawn as a prompt and recorded in the transcript, as a typed prompt is.
#[cfg(feature = "tui")]
fn steer_into(
    conversation: &mut [ModelMessage],
    composer: &mut tui::Composer,
    transcript: &mut tui::Transcript,
    colour: bool,
) -> io::Result<()> {
    let Some(results) = conversation
        .last_mut()
        .filter(|message| message.role == ModelRole::User)
        .filter(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ModelContent::ToolResult { .. }))
        })
    else {
        return Ok(());
    };
    let steering = composer.take_steering();
    if steering.is_empty() {
        return Ok(());
    }
    let mut terminal = io::stdout();
    for line in steering {
        write!(terminal, "{}", composer.commit(&line, colour))?;
        transcript.push_user(&line);
        results.content.push(ModelContent::Text {
            text: format!("The operator added this while you were working: {line}"),
        });
    }
    terminal.flush()
}

/// Erase a running tool's card and the composer drawn under it, leaving the
/// cursor where the card began.
///
/// The cursor rests in the composer, below everything it draws above its input
/// — the live status and any queued follow-ups — so the composer is erased by
/// its own count first and only then is the card climbed over. Climbing the
/// card's height from the input row stopped short by those rows and left them,
/// and the card's first rows, in the scrollback.
#[cfg(feature = "tui")]
fn erase_card(composer: &mut tui::Composer, card_rows: usize) -> String {
    let mut erase = composer.clear();
    erase.push_str(&format!("\x1b[{card_rows}A\r\x1b[J"));
    erase
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
        // Rounded up to a character boundary: output is whatever a command
        // printed, and a byte offset can land inside a ✓ or an emoji.
        let oldest = live.ceil_char_boundary(live.len() - KEEP_BYTES);
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
            // Shift+Tab, Ctrl+T and `/effort` change the controls now, never
            // a follow-up task: queued, a drafted chat line would be answered
            // as if it were a second user message. The Codex CLI child keeps
            // the effort it started with; the footer shows the change now.
            action if live_control(&action, board.approval) => typed = true,
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
            // Taken by `live_control` above.
            tui::Action::CycleMode | tui::Action::CycleEffort | tui::Action::None => {}
        }
    }
    Ok(typed)
}

/// The status row under a running turn: model, effort, mode, directory.
///
/// Built once when the turn starts, and again whenever Shift+Tab, Ctrl+T or
/// `/effort` changes what it names, so a change made mid-turn shows at once
/// instead of when the turn ends.
#[cfg(feature = "tui")]
pub(crate) struct Footer<'a> {
    approval: Option<&'a approval::ApprovalCell>,
    branch: Option<String>,
    width: usize,
    colour: bool,
    held: std::cell::RefCell<HeldFooter>,
}

#[cfg(feature = "tui")]
struct HeldFooter {
    state: tui::TuiState,
    mode: approval::ApprovalMode,
    effort: Option<Effort>,
    row: String,
}

#[cfg(feature = "tui")]
impl<'a> Footer<'a> {
    /// A footer that follows `approval`, drawn from a copy of `state`.
    pub(crate) fn live(
        state: &tui::TuiState,
        approval: &'a approval::ApprovalCell,
        branch: Option<String>,
        width: usize,
        colour: bool,
    ) -> Self {
        let state = state.clone();
        let row = state.status_row(width, colour, branch.as_deref());
        Self {
            held: std::cell::RefCell::new(HeldFooter {
                state,
                mode: approval.get(),
                effort: approval.effort(),
                row,
            }),
            approval: Some(approval),
            branch,
            width,
            colour,
        }
    }

    /// A footer that never changes, for a caller with no live controls.
    #[cfg(test)]
    pub(crate) fn fixed(row: &str) -> Self {
        Self {
            approval: None,
            branch: None,
            width: 0,
            colour: false,
            held: std::cell::RefCell::new(HeldFooter {
                state: tui::TuiState::new(String::new(), SessionId::new()),
                mode: approval::ApprovalMode::Default,
                effort: None,
                row: row.to_owned(),
            }),
        }
    }

    pub(crate) fn row(&self) -> String {
        let mut held = self.held.borrow_mut();
        if let Some(approval) = self.approval {
            let (mode, effort) = (approval.get(), approval.effort());
            if (mode, effort) != (held.mode, held.effort) {
                held.state.set_approval_mode(mode.label());
                held.state.set_effort(effort);
                held.row = held
                    .state
                    .status_row(self.width, self.colour, self.branch.as_deref());
                held.mode = mode;
                held.effort = effort;
            }
        }
        held.row.clone()
    }
}

/// Draws the rows a provider turn produces, above the live composer.
#[cfg(feature = "tui")]
pub(crate) struct Painter<'a> {
    pub(crate) colour: bool,
    pub(crate) footer: &'a Footer<'a>,
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
        frame.push_str(&composer.render_turn(
            self.width.get(),
            self.colour,
            &status,
            &self.footer.row(),
        ));
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
    /// Every reasoning delta this round, whole, for the transcript to keep
    /// what was drawn line by line.
    thought: String,
    thinking_open: bool,
    /// The last reasoning row drawn was blank, so the next blank one is not:
    /// a model that leaves two blank lines between thoughts gets one.
    thinking_blank: bool,
    /// Rows of a table in the reasoning, held until the table ends so it is
    /// drawn as a grid rather than as raw pipes.
    thinking_table: String,
    /// Rows drawn for the current Markdown response block.
    live_lines: usize,
    /// Part of this answer already settled into scrollback, so the live rest
    /// draws without a second `✦`.
    continued: bool,
    last_frame: Option<std::time::Instant>,
    /// When the text now in `pending` started waiting for a word boundary.
    waiting_since: Option<std::time::Instant>,
    /// `(width, rows, read at)`. Read through the terminal rather than a
    /// subprocess, but still cached: the rows already on screen were laid out
    /// for one size, and reflowing them on every frame is not what a frame is
    /// for.
    size: Option<(usize, usize, std::time::Instant)>,
    /// A tool call whose arguments are still arriving. Only one of it and a
    /// live answer is ever on screen: each closes the other before it draws.
    draft: Option<Draft>,
}

/// The card for a tool call the model is still writing. Display only: the
/// fragments are never parsed into a call, so nothing here can run.
#[cfg(feature = "tui")]
struct Draft {
    index: usize,
    name: String,
    raw: String,
    lines: usize,
    started: std::time::Instant,
    last_frame: Option<std::time::Instant>,
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
        footer: &Footer<'_>,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        let width = tui::terminal_width();
        self.thinking.push_str(text);
        self.thought.push_str(text);
        if !self.thinking_open {
            if self.thinking.trim().is_empty() {
                return Ok(());
            }
            self.thinking_open = true;
            self.thinking_blank = true;
            // The header opens a block even though it is one row, so it
            // takes the gap the rows inside the block do not.
            stream_row_after(
                terminal,
                composer,
                colour,
                footer,
                status,
                modern_gap(),
                &tui::thinking_box_top(width, colour),
            )?;
        }
        for line in drain_lines(&mut self.thinking) {
            if tui::is_table_row(&line) {
                // The line keeps its own newline.
                self.thinking_table.push_str(&line);
                continue;
            }
            self.flush_thinking_table(terminal, composer, colour, footer, status)?;
            self.thinking_row(terminal, composer, colour, footer, status, &line)?;
        }
        Ok(())
    }

    /// One reasoning line, inside the block: no gap of its own, and a blank
    /// line only between thoughts, never two in a row.
    fn thinking_row(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
        line: &str,
    ) -> io::Result<()> {
        let blank = line.trim().is_empty();
        if blank && self.thinking_blank {
            return Ok(());
        }
        self.thinking_blank = blank;
        let row = tui::thinking_box_row(tui::terminal_width(), colour, line);
        stream_row_after(terminal, composer, colour, footer, status, "", &row)
    }

    /// Draw a held reasoning table, once the line after it shows it ended.
    fn flush_thinking_table(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<()> {
        let table = std::mem::take(&mut self.thinking_table);
        for row in tui::thinking_table_rows(tui::terminal_width(), colour, &table) {
            stream_row_after(terminal, composer, colour, footer, status, "", &row)?;
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
        footer: &Footer<'_>,
        status: &str,
        text: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        self.end_draft(terminal, composer)?;
        self.pending.push_str(text);
        self.waiting_since
            .get_or_insert_with(std::time::Instant::now);
        Ok(())
    }

    /// Open a live card for a tool call the model has started writing,
    /// settling the answer above it first.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tool_started(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
        index: usize,
        name: String,
    ) -> io::Result<()> {
        self.close(terminal, composer, colour, footer, status)?;
        self.draft = Some(Draft {
            index,
            name,
            raw: String::new(),
            lines: 0,
            started: std::time::Instant::now(),
            last_frame: None,
        });
        self.draw_draft(terminal, composer, colour, footer, status)
    }

    /// Grow the draft card by one argument fragment, redrawing at most once
    /// a frame.
    // ponytail: parallel calls show only the latest one started; fragments
    // for any other index are dropped from the display, never from the call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tool_delta(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
        index: usize,
        fragment: &str,
    ) -> io::Result<()> {
        let Some(draft) = self.draft.as_mut().filter(|draft| draft.index == index) else {
            return Ok(());
        };
        draft.raw.push_str(fragment);
        if draft.last_frame.is_some_and(|last| last.elapsed() < FRAME) {
            return Ok(());
        }
        self.draw_draft(terminal, composer, colour, footer, status)
    }

    fn draw_draft(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<()> {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        let (width, _) = self.size();
        let Some(draft) = self.draft.as_mut() else {
            return Ok(());
        };
        let fields = partial_strings(&draft.raw);
        let field = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| fields.iter().find(|(name, _)| name == key))
                .map_or("", |(_, value)| value.as_str())
        };
        let elapsed = draft.started.elapsed();
        let spin = usize::try_from(elapsed.as_millis() / 100).unwrap_or(0);
        let card = tui::tool_running_box(
            width,
            colour,
            &tui::RunningToolState {
                name: &draft.name,
                summary: field(&["path", "file_path", "command"]),
                frame: FRAMES[spin % FRAMES.len()],
                elapsed_ms: elapsed.as_millis(),
                live_output: field(&["content", "input", "patch", "new_string", "command"]),
                expanded: true,
                drafting: true,
            },
        );
        let mut frame = composer.clear();
        for _ in 0..draft.lines {
            frame.push_str("\x1b[1A\r\x1b[K");
        }
        for line in &card {
            frame.push_str(line);
            frame.push('\n');
        }
        frame.push_str(&composer.render_turn(width, colour, status, &footer.row()));
        draft.lines = card.len();
        draft.last_frame = Some(std::time::Instant::now());
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    }

    /// Take the draft card down: the call it drew has completed and the
    /// host draws its real card, or the round ended without it.
    pub(crate) fn end_draft(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
    ) -> io::Result<()> {
        match self.draft.take() {
            Some(draft) if draft.lines > 0 => erase_live_response(terminal, composer, draft.lines),
            _ => Ok(()),
        }
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
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<bool> {
        // The draft's spinner and clock keep moving while its arguments stall.
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.last_frame.is_none_or(|last| last.elapsed() >= FRAME))
        {
            self.draw_draft(terminal, composer, colour, footer, status)?;
            return Ok(true);
        }
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
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<()> {
        let (width, rows) = self.size();
        let composer_rows = composer
            .render_turn(width, colour, status, &footer.row())
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
                let (width, rows) = tui::terminal_dimensions();
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
        footer: &Footer<'_>,
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
        frame.push_str(&composer.render_turn(width, colour, status, &footer.row()));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    }

    /// Close the round: reveal whatever is still queued, finish whatever box
    /// is open and settle the answer.
    pub(crate) fn close(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<()> {
        self.close_thinking(terminal, composer, colour, footer, status)?;
        self.end_draft(terminal, composer)?;
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
        self.end_draft(terminal, composer)?;
        if self.live_lines > 0 {
            erase_live_response(terminal, composer, self.live_lines)?;
            self.live_lines = 0;
        }
        Ok(())
    }

    /// Give the turn the reasoning this round showed, for the transcript.
    pub(crate) fn hand_over(&mut self, mut turn: Turn) -> Turn {
        let thought = std::mem::take(&mut self.thought);
        if !thought.trim().is_empty() {
            turn.thinking = Some(thought);
        }
        turn
    }

    /// Close the reasoning box if it is open, flushing the line it was part
    /// way through.
    pub(crate) fn close_thinking(
        &mut self,
        terminal: &mut dyn Write,
        composer: &mut tui::Composer,
        colour: bool,
        footer: &Footer<'_>,
        status: &str,
    ) -> io::Result<()> {
        if !self.thinking_open {
            return Ok(());
        }
        self.thinking_open = false;
        let width = tui::terminal_width();
        if tui::is_table_row(&self.thinking) {
            let row = std::mem::take(&mut self.thinking);
            self.thinking_table.push_str(&row);
        }
        self.flush_thinking_table(terminal, composer, colour, footer, status)?;
        if !self.thinking.trim().is_empty() {
            let line = std::mem::take(&mut self.thinking);
            self.thinking_row(terminal, composer, colour, footer, status, &line)?;
        }
        // The modern box has no bottom edge; a bare row for it would only add
        // to the gap the next block brings.
        let bottom = tui::thinking_box_bottom(width, colour);
        if bottom.is_empty() {
            return Ok(());
        }
        stream_row(terminal, composer, colour, footer, status, &bottom)
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

/// The string fields of a JSON object that may still be arriving, decoded as
/// far as they go: `(key, value)` for each top-level string value, the last
/// one possibly cut short.
///
/// Display only. A tool call becomes runnable solely as a complete, parsed
/// `ToolCallCompleted`; nothing read here ever reaches a dispatch.
#[cfg(feature = "tui")]
pub(crate) fn partial_strings(raw: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let Some(open) = raw.find('{') else {
        return pairs;
    };
    let mut chars = raw[open + 1..].chars();
    let mut depth = 1usize;
    let mut key: Option<String> = None;
    let mut value_next = false;
    while let Some(character) = chars.next() {
        match character {
            '"' => {
                let (text, closed) = partial_string(&mut chars);
                if depth == 1 {
                    if value_next {
                        if let Some(key) = key.take() {
                            pairs.push((key, text));
                        }
                        value_next = false;
                    } else {
                        key = Some(text);
                    }
                }
                if !closed {
                    break;
                }
            }
            ':' if depth == 1 => value_next = key.is_some(),
            ',' if depth == 1 => {
                key = None;
                value_next = false;
            }
            '{' | '[' => {
                depth += 1;
                key = None;
                value_next = false;
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    pairs
}

/// One JSON string after its opening quote, and whether its closing quote
/// arrived. An escape cut off by the end of the input is left out.
// ponytail: a `\u` surrogate pair decodes as two `�`; models send raw UTF-8
// for anything outside the BMP, so pairing them has not been needed.
#[cfg(feature = "tui")]
fn partial_string(chars: &mut std::str::Chars<'_>) -> (String, bool) {
    let mut text = String::new();
    while let Some(character) = chars.next() {
        match character {
            '"' => return (text, true),
            '\\' => match chars.next() {
                Some('n') => text.push('\n'),
                Some('t') => text.push('\t'),
                Some('r') => text.push('\r'),
                Some('b') => text.push('\u{8}'),
                Some('f') => text.push('\u{c}'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() < 4 {
                        return (text, false);
                    }
                    text.push(
                        u32::from_str_radix(&hex, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or(char::REPLACEMENT_CHARACTER),
                    );
                }
                Some(other) => text.push(other),
                None => return (text, false),
            },
            other => text.push(other),
        }
    }
    (text, false)
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
        // A table row is never the last line of a settled piece: rows cut
        // away from their header are no longer a table to the renderer, and
        // print as raw pipes. The break before the table is taken instead.
        let table_row = fence.is_none() && trimmed.starts_with('|');
        if !line.ends_with('\n') || opened_here || table_row {
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
    footer: &Footer<'_>,
    status: &str,
    row: &str,
) -> io::Result<()> {
    stream_row_after(
        terminal,
        composer,
        colour,
        footer,
        status,
        block_gap(row),
        row,
    )
}

/// [`stream_row`] with the gap above the row chosen by the caller, for a row
/// whose place in a block decides it rather than its own height.
#[cfg(feature = "tui")]
pub(crate) fn stream_row_after(
    terminal: &mut dyn Write,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &Footer<'_>,
    status: &str,
    gap: &str,
    row: &str,
) -> io::Result<()> {
    let mut frame = composer.clear();
    frame.push_str(gap);
    frame.push_str(row);
    frame.push('\n');
    frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, &footer.row()));
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
            // A follow-up was queued to run after this turn, not instead of
            // stopping it, so the stop takes the queue with it.
            composer.take_held();
            outcome.queued.clear();
            outcome.interrupted = true;
            return Typed::Interrupted;
        }
        if key == tui::Key::Tab {
            if queue_draft(composer) {
                typed = Typed::Redraw;
            }
            continue;
        }
        match composer.press(key) {
            // Shift+Tab changes authority immediately; it never becomes a
            // model prompt or a queued follow-up. Nor do Ctrl+T or `/effort`.
            action if live_control(&action, approval) => typed = Typed::Redraw,
            // Enter steers the running turn. Held by the composer, not this
            // round's outcome: the composer outlives every round of the turn,
            // so the line is still there for the next round, and it is drawn
            // above the input while it waits.
            tui::Action::Submit(line) if !line.trim().is_empty() => {
                keep_sent(composer, line, true);
                typed = Typed::Redraw;
            }
            tui::Action::Expand | tui::Action::Submit(_) | tui::Action::Redraw => {
                typed = Typed::Redraw
            }
            tui::Action::Quit => outcome.quit = true,
            // Taken by `live_control` above.
            tui::Action::CycleMode | tui::Action::CycleEffort | tui::Action::None => {}
        }
    }
    typed
}

/// Apply a key or line that changes the operator's controls while a turn
/// runs — Shift+Tab, Ctrl+T, or `/effort <level>` — answering whether the
/// action was one.
#[cfg(feature = "tui")]
fn live_control(action: &tui::Action, approval: &approval::ApprovalCell) -> bool {
    match action {
        tui::Action::CycleMode => {
            cycle_approval_mode(approval);
            true
        }
        tui::Action::CycleEffort => {
            approval.cycle_effort();
            true
        }
        tui::Action::Submit(line) => live_effort(line, approval),
        _ => false,
    }
}

/// Apply `/effort <level>` typed while a turn runs, answering whether the
/// line was one.
///
/// Queued like any other line, it would only change the effort after the
/// turn it was meant for; applied here, the turn's next request carries it.
#[cfg(feature = "tui")]
pub(crate) fn live_effort(line: &str, approval: &approval::ApprovalCell) -> bool {
    let mut words = line.split_whitespace();
    let (Some("/effort"), Some(level), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    let choices = approval.effort_choices();
    match tui::resolve_effort_answer(level, approval.effort(), &choices) {
        Ok(effort) if approval.takes_effort() && choices.contains(&effort) => {
            approval.set_effort(effort);
            true
        }
        // Not a level, or not one this model runs at: queued as typed, so
        // `/effort` answers it after the turn with its usual explanation.
        _ => false,
    }
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
    let (model, effort) = routed_model(resolved, route, effort);
    Ok(CanonicalModelRequest {
        model: ModelKey {
            provider: route.provider.clone(),
            model: model.clone(),
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
        max_output_tokens: resolved.endpoint.output_tokens_for(&model),
        effort,
        // One turn can take several requests, one per round of tool calls. The
        // round is part of the key, because a retry must repeat its own
        // request rather than collapse into the one before it.
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(format!("{turn}-{round}"))
            .map_err(io::Error::other)?,
    })
}

/// The model a request on `route` goes to, and the effort it carries.
///
/// Only a level this model offers is sent: none to a model without the knob,
/// and anything else clamped down to the nearest it takes. A model listed
/// once per effort is routed by its base name, and the request goes to the
/// variant the effort names, which is the only ID such a provider serves.
#[cfg(feature = "tui")]
pub(crate) fn routed_model(
    resolved: &provider::Resolved,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
) -> (String, Option<Effort>) {
    let effort = picker::remembered::effort_profile(&resolved.endpoint, &route.model).clamp(effort);
    let model = tui::variant_for(&resolved.endpoint.models, &route.model, effort);
    (model, effort)
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
        let sent = rows.clone();
        // A decoder that panics would otherwise close the channel the same way
        // a finished stream does, and the turn would end as if it had answered.
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let started = match arsy_kernel::provider::stream_with_retry(
                provider.as_ref(),
                &request,
                &mut std::thread::sleep,
            ) {
                // A model that has no reasoning knob may refuse the field
                // outright. The request is sent once more without it, and the
                // model is remembered for the rest of the session.
                Err(error) if refused_effort(&request, &error) => {
                    picker::remembered::learn_no_effort(
                        &request.model.provider,
                        &request.model.model,
                    );
                    let request = CanonicalModelRequest {
                        effort: None,
                        ..request.clone()
                    };
                    arsy_kernel::provider::stream_with_retry(
                        provider.as_ref(),
                        &request,
                        &mut std::thread::sleep,
                    )
                }
                started => started,
            };
            let stream = match started {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = sent.send(Err(error));
                    return;
                }
            };
            for event in stream {
                let Some(message) = streamed(event) else {
                    continue;
                };
                let failed = message.is_err();
                if sent.send(message).is_err() || failed {
                    return;
                }
            }
        }));
        if run.is_err() {
            let _ = rows.send(Err(arsy_kernel::provider::ProviderError::Decode(
                "the response stream stopped unexpectedly".to_owned(),
            )));
        }
    });
    events
}

/// Record what a round drew — its reasoning, then its text — so a repaint
/// draws every round again, not only the turn's last answer.
#[cfg(feature = "tui")]
fn record_round(transcript: &mut tui::Transcript, outcome: &mut Turn) {
    if let Some(thinking) = outcome.thinking.take() {
        transcript.push_thinking(&thinking);
    }
    if !outcome.response.trim().is_empty() {
        transcript.push_assistant(&outcome.response);
    }
    outcome.recorded = true;
}

/// Draw the MCP failures and log rows the session connector reported, as one
/// panel, and record it so a resize draws it again.
#[cfg(feature = "tui")]
fn show_mcp_panel(emitter: &mut Emitter, transcript: &mut tui::Transcript, colour: bool) {
    let (failures, log) = emitter.take_mcp();
    if failures.is_empty() && log.is_empty() {
        return;
    }
    let panel = tui::mcp_panel(colour, tui::terminal_width(), &failures, &log);
    let _ = writeln!(
        io::stdout(),
        "{}{}{panel}{}",
        modern_gap(),
        tui::DISABLE_AUTOWRAP,
        tui::ENABLE_AUTOWRAP
    );
    transcript.push_mcp(failures, log);
}

/// A stream that has started and then goes quiet this long is a dropped
/// connection (a laptop that slept, a NAT that forgot it), not a slow answer.
#[cfg(feature = "tui")]
const STALLED: std::time::Duration = std::time::Duration::from_secs(300);

/// The next streamed row, or a transport failure once a started stream has
/// sent nothing for [`STALLED`]. Before the first event the model may still be
/// thinking unseen, so the bound only applies once something has arrived.
#[cfg(feature = "tui")]
fn receive_watched(
    events: &std::sync::mpsc::Receiver<Result<Streamed, arsy_kernel::provider::ProviderError>>,
    wait: std::time::Duration,
    started: bool,
    last_event: &mut std::time::Instant,
) -> Result<Result<Streamed, arsy_kernel::provider::ProviderError>, std::sync::mpsc::RecvTimeoutError>
{
    let received = events.recv_timeout(wait);
    if received.is_ok() {
        *last_event = std::time::Instant::now();
    } else if started && last_event.elapsed() >= STALLED {
        return Ok(Err(arsy_kernel::provider::ProviderError::Transport(
            format!(
                "the response stream sent nothing for {}s",
                STALLED.as_secs()
            ),
        )));
    }
    received
}

/// Whether a request was refused for the reasoning effort it carried.
#[cfg(feature = "tui")]
fn refused_effort(
    request: &CanonicalModelRequest,
    error: &arsy_kernel::provider::ProviderError,
) -> bool {
    let arsy_kernel::provider::ProviderError::InvalidRequest(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    request.effort.is_some()
        && ["reasoning", "thinking", "effort"]
            .iter()
            .any(|word| message.contains(word))
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
        Ok(ModelEvent::Reasoning { state }) => Ok(Streamed::Reasoning(state)),
        Ok(ModelEvent::Usage {
            input_tokens,
            output_tokens,
        }) => Ok(Streamed::Usage {
            input_tokens,
            output_tokens,
        }),
        Ok(ModelEvent::ToolCallStarted { index, name, .. }) => {
            Ok(Streamed::ToolStarted { index, name })
        }
        Ok(ModelEvent::ToolCallDelta { index, fragment }) => {
            Ok(Streamed::ToolDelta { index, fragment })
        }
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

/// Show a compaction and answer what to record of it.
///
/// Drawn as a strip that stays in the transcript, so a resize or an expand
/// keeps it, and torn down around the composer like any other row. The
/// returned detail is what `context.compacted` records.
#[cfg(feature = "tui")]
pub(crate) fn report_trim(
    terminal: &mut dyn Write,
    colour: bool,
    trimmed: &arsy_code::agent::budget::Trimmed,
    manual: bool,
    transcript: &mut tui::Transcript,
    composer: &mut tui::Composer,
    progress: &mut CompactionProgress,
) -> io::Result<Option<Value>> {
    // The live row, when one was drawn, gives its place to the result.
    let settled = progress.settle(terminal)?;
    if !trimmed.changed() {
        if settled {
            composer.invalidate();
        }
        return Ok(None);
    }
    let compacted = tui::Compacted {
        messages: trimmed.summarized,
        elided: trimmed.elided,
        before: trimmed.before,
        after: trimmed.after,
        manual,
    };
    let lead = if settled {
        String::new()
    } else {
        format!("{}{}", composer.clear(), modern_gap())
    };
    writeln!(
        terminal,
        "{lead}{}",
        tui::compaction_row(&compacted, colour)
    )?;
    terminal.flush()?;
    composer.invalidate();
    transcript.push_compaction(compacted);
    Ok(Some(compaction_detail(trimmed, manual)))
}

/// The live row of a compaction in progress, drawn in place at each step and
/// replaced by the strip that reports the result.
#[cfg(feature = "tui")]
pub(crate) struct CompactionProgress {
    tokens: u32,
    budget: Option<u32>,
    frame: usize,
    drawn: bool,
}

#[cfg(feature = "tui")]
impl CompactionProgress {
    /// Progress for fitting `tokens` into `budget`, or for `/compact` with
    /// no budget forcing it.
    pub(crate) fn new(tokens: u32, budget: Option<u32>) -> Self {
        Self {
            tokens,
            budget,
            frame: 0,
            drawn: false,
        }
    }

    /// Draw the row for `step`. The first draw clears the composer and
    /// opens the block; the first step shown is always the measure, so the
    /// row never starts part way through.
    pub(crate) fn show(
        &mut self,
        terminal: &mut dyn Write,
        composer: Option<&mut tui::Composer>,
        colour: bool,
        step: arsy_tui::widget::CompactionStep,
    ) -> io::Result<()> {
        if !self.drawn {
            if let Some(composer) = composer {
                write!(terminal, "{}", composer.clear())?;
            }
            write!(terminal, "{}", modern_gap())?;
            self.drawn = true;
            if step != arsy_tui::widget::CompactionStep::Measuring {
                self.draw(
                    terminal,
                    colour,
                    arsy_tui::widget::CompactionStep::Measuring,
                )?;
            }
        }
        self.draw(terminal, colour, step)
    }

    fn draw(
        &mut self,
        terminal: &mut dyn Write,
        colour: bool,
        step: arsy_tui::widget::CompactionStep,
    ) -> io::Result<()> {
        let row = arsy_tui::widget::compaction_progress(
            step,
            self.tokens,
            self.budget,
            self.frame,
            tui::terminal_width(),
        );
        self.frame += 1;
        write!(terminal, "\r\x1b[K{}", tui::render_row(colour, &row))?;
        terminal.flush()
    }

    /// Erase the row, answering whether there was one; the cursor is left at
    /// the start of the line it held.
    pub(crate) fn settle(&mut self, terminal: &mut dyn Write) -> io::Result<bool> {
        if !std::mem::take(&mut self.drawn) {
            return Ok(false);
        }
        write!(terminal, "\r\x1b[K")?;
        Ok(true)
    }
}

/// The step a budget stage is shown as.
#[cfg(feature = "tui")]
fn compaction_step(stage: arsy_code::agent::budget::Stage) -> arsy_tui::widget::CompactionStep {
    match stage {
        arsy_code::agent::budget::Stage::Eliding => arsy_tui::widget::CompactionStep::Eliding,
        arsy_code::agent::budget::Stage::Folding => arsy_tui::widget::CompactionStep::Folding,
    }
}

/// What `context.compacted` records of one compaction.
#[cfg(feature = "tui")]
pub(crate) fn compaction_detail(
    trimmed: &arsy_code::agent::budget::Trimmed,
    manual: bool,
) -> Value {
    json!({
        "trigger": if manual { "manual" } else { "budget" },
        "messages": trimmed.summarized,
        "elided": trimmed.elided,
        "before_tokens": trimmed.before,
        "after_tokens": trimmed.after,
        "summary": trimmed.summary,
    })
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
    footer: &'a Footer<'a>,
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
    // A new effect can invalidate an earlier read, so reads are reused only
    // until the next effectful call. An effectful call is never reused: an
    // operator who asks for the tests again wants them run again.
    let observational = runtime.is_observational(call.name, call.arguments);
    if !observational {
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
            if result.success && observational {
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
    footer: &Footer<'_>,
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
                    // A hook's question is about the hook, not the path.
                    outside: None,
                    runtime,
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
    // A command the operator allowlisted is their decision, not the
    // denylist's: it runs in Auto without the per-action review.
    let allowlisted = name == "bash"
        && arguments["command"]
            .as_str()
            .is_some_and(|command| approval.allows_command(command));
    let safety = (approval.get() == approval::ApprovalMode::Auto
        && !allowlisted
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
            outside: runtime.outside_directory(&request),
            runtime,
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
        composer, footer, approval,
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
    /// The directory, and how much of it, that approving "always" allows for
    /// the session, when the call reaches outside the workspace and every
    /// added directory.
    outside: Option<(PathBuf, arsy_code::operations::DirectoryAccess)>,
    runtime: &'a arsy_code::agent::ToolRuntime,
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
            format!(
                "Blocked in auto mode: {}. Take another approach that avoids this action, \
                 or tell the operator it needs to be run by hand.",
                review.reasons.join("; ")
            ),
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
    let command = (name == "bash")
        .then(|| asking.arguments["command"].as_str())
        .flatten();
    match mode_decision(
        asking.approval,
        name,
        command,
        force_approval,
        asking.outside.is_some(),
    ) {
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
            let facts = outside_facts(
                policy_approval_facts(name, asking.summary, &authorization),
                asking.outside.as_ref(),
            );
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
                    remember_always(&asking, &authorization);
                    if let Some(command) = command {
                        asking.approval.remember_command(command);
                    }
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

/// Outside every directory, "always" is an answer about the directory, so the
/// card shows the directory it would allow rather than one file's grant.
#[cfg(feature = "tui")]
fn outside_facts(
    mut facts: ApprovalFacts,
    outside: Option<&(PathBuf, arsy_code::operations::DirectoryAccess)>,
) -> ApprovalFacts {
    if let Some((directory, access)) = outside {
        let access = match access {
            arsy_code::operations::DirectoryAccess::Read => "read",
            arsy_code::operations::DirectoryAccess::Full => "read and edit",
        };
        facts.scope = format!("{} for this session ({access})", directory.display());
        facts.rule_approval = true;
    }
    facts
}

/// Keep an "always" answer: the directory for an outside call, the exact
/// grants otherwise.
#[cfg(feature = "tui")]
fn remember_always(asking: &Asking<'_>, authorization: &arsy_code::agent::Authorization) {
    match &asking.outside {
        Some((directory, access)) => {
            asking
                .approval
                .allow_directory(asking.runtime, directory, *access)
        }
        None => asking.approval.remember(authorization),
    }
}

/// What the approval mode says about a call policy left for approval.
///
/// A safety review that requires approval always asks. Otherwise Accept
/// Edits still asks before a shell command, unless the operator's own
/// configuration or an earlier "always" covers that command.
#[cfg(feature = "tui")]
fn mode_decision(
    approval: &approval::ApprovalCell,
    name: &str,
    command: Option<&str>,
    force_approval: bool,
    outside: bool,
) -> approval::Decision {
    let mode = approval.get();
    // Outside the workspace and every added directory, the mode's standing
    // answer does not apply; `requested` already names the path, so a
    // refusal here says why.
    if let Some(decision) = outside.then(|| approval::decide_outside(mode)).flatten() {
        return decision;
    }
    if force_approval {
        return approval::Decision::Ask;
    }
    match approval::decide(mode, name) {
        approval::Decision::Ask
            if mode == approval::ApprovalMode::AcceptEdits
                && command.is_some_and(|command| approval.allows_command(command)) =>
        {
            approval::Decision::Approve
        }
        decision => decision,
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
    footer: &Footer<'_>,
    approval: &approval::ApprovalCell,
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
        // A tool that panics is a failed call the model can read, not a lost
        // worker that ends the turn with the call left unanswered.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_runtime.dispatch(&worker_name, &worker_request, &grants, started)
        }))
        .unwrap_or_else(|_| {
            arsy_code::agent::ToolResult::refused(
                &worker_name,
                "the tool stopped unexpectedly before it finished; check the workspace \
                 state before trying it again",
            )
        });
        let _ = sender.send(result);
    });

    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let mut frame = 0usize;
    let elapsed = std::time::Instant::now();
    let mut cancelled = false;
    let mut expanded = tui::opens_expanded();
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
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                state: &tui::RunningToolState<'_>,
                drawn_rows: usize,
                tick: usize|
     -> io::Result<usize> {
        let mut card = tui::tool_running_box(tui::terminal_width(), colour, state);
        // The gap above the card is one of its rows, so it is climbed over
        // with it and the finished card lands where this one stood.
        if !modern_gap().is_empty() {
            card.insert(0, String::new());
        }
        // The composer is drawn with the card, not instead of it: the input
        // line is where the operator types the next turn while this one runs,
        // and a card that replaced it read as the session being busy at them.
        // The previous card is climbed over too, or every tick left a copy of
        // it in the scrollback.
        let mut frame = composer.clear();
        for _ in 0..drawn_rows {
            frame.push_str("\x1b[1A\r\x1b[K");
        }
        frame.push_str(tui::DISABLE_AUTOWRAP);
        frame.push_str(&card.iter().map(|l| format!("{l}\n")).collect::<String>());
        frame.push_str(tui::ENABLE_AUTOWRAP);
        frame.push_str(&composer.render_turn(
            tui::terminal_width(),
            colour,
            &tui::turn_status(
                colour,
                tui::TurnPhase::Working,
                std::time::Duration::from_millis(
                    u64::try_from(state.elapsed_ms).unwrap_or(u64::MAX),
                ),
                tick,
                0,
            ),
            &footer.row(),
        ));
        write!(terminal, "{frame}")?;
        terminal.flush()?;
        Ok(card.len())
    };
    let initial_lines = draw(terminal, composer, &initial_state, 0, 0)?;
    let mut last_rendered_lines = initial_lines;
    loop {
        let was_expanded = expanded;
        if absorb_live_keys(
            keys,
            decoder,
            composer,
            approval,
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
        // Ctrl+O redraws at once rather than on the next tick, so the card
        // answers the key instead of seeming to ignore it.
        let tick = std::time::Duration::from_millis(80) * u32::from(was_expanded == expanded);
        match receiver.recv_timeout(tick) {
            Ok(result) => {
                runtime.set_output_sink(None);
                // The card is erased; the composer the next frame draws is
                // positioned above where the card used to be.
                if last_rendered_lines > 0 {
                    write!(terminal, "{}", erase_card(composer, last_rendered_lines))?;
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
                last_rendered_lines = draw(terminal, composer, &state, last_rendered_lines, frame)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                runtime.set_output_sink(None);
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
    // The gap above the dialog is one of its rows, erased with it, or every
    // approval leaves a blank row above the card that follows.
    let frame = format!("{}{}", modern_gap(), dialog.render(width, colour));
    let mut rendered_lines = frame.lines().count();
    writeln!(terminal, "{frame}")?;
    terminal.flush()?;
    approval.open();
    let title = dialog.title.clone();
    loop {
        match keys.recv() {
            Ok(byte) => match decoder.feed(byte) {
                Some(key) => {
                    if let Some(result) = dialog.handle_key(key) {
                        write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
                        terminal.flush()?;
                        match result {
                            tui::AskDialogResult::Approve { note }
                            | tui::AskDialogResult::ApprovePlan { note, .. } => {
                                return Ok(Answer::Yes { note })
                            }
                            // A rule covers the one effect it was granted for,
                            // never the mode: approving one call must not quietly
                            // approve every later one.
                            tui::AskDialogResult::ApproveRule { note } => {
                                return Ok(Answer::Rule { note })
                            }
                            tui::AskDialogResult::Revise { .. } => {}
                            tui::AskDialogResult::Deny { note } => return Ok(Answer::No { note }),
                            // The mode changes now and governs the calls after
                            // this one. This call was asked under the old mode
                            // and still needs its answer: settling it by the
                            // new mode here would skip the Auto safety review
                            // the caller runs before ever asking.
                            tui::AskDialogResult::CycleMode => {
                                let mode = cycle_approval_mode(approval);
                                dialog.title =
                                    format!("{title} · mode {} from the next call", mode.label());
                            }
                            tui::AskDialogResult::Cancel => return Ok(Answer::Stop),
                        }
                        // Still open: drawn again where it was just erased.
                        let frame = format!("{}{}", modern_gap(), dialog.render(width, colour));
                        writeln!(terminal, "{frame}")?;
                        terminal.flush()?;
                        rendered_lines = frame.lines().count();
                    } else {
                        let frame = format!("{}{}", modern_gap(), dialog.render(width, colour));
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
    footer: &Footer<'_>,
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
    frame.push_str(&composer.render_turn(width, colour, status, &footer.row()));
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
    footer: &Footer<'_>,
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
    if matches!(
        error,
        arsy_kernel::provider::ProviderError::IncompleteToolArguments(_)
    ) {
        // No tool calls from a failed stream are dispatched. Give a malformed
        // provider completion one fresh request before failing the turn.
        return native_status(
            resolved,
            config,
            runtime,
            conversation,
            route,
            effort,
            turn,
            round + config.max_tool_rounds(),
            colour,
            footer,
            keys,
            decoder,
            composer,
            approval,
        );
    }
    if !is_stale_oauth_token(error, resolved.source) {
        return Ok(outcome);
    }
    let provider_id = resolved.endpoint.id.clone();
    // The provider refused the token, so renew it whatever its expiry says;
    // a login that cannot be renewed is gone, and the operator is told to
    // sign in again rather than shown the provider's bare refusal.
    let refused = refusal(error);
    if let Err(cause) = provider::renew_rejected_login(&resolved.endpoint) {
        return Ok(login_lost_turn(outcome, &provider_id, &refused, &cause));
    }
    let Ok(mut refreshed) = provider::resolve(config, Some(&resolved.endpoint.id)) else {
        return Ok(outcome);
    };
    refreshed
        .endpoint
        .context_windows
        .extend(resolved.endpoint.context_windows.clone());
    refreshed
        .endpoint
        .input_limits
        .extend(resolved.endpoint.input_limits.clone());
    refreshed
        .endpoint
        .output_limits
        .extend(resolved.endpoint.output_limits.clone());
    *resolved = refreshed;
    let retried = native_status(
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
    // Refused again with a token just issued: the account itself is the
    // problem, and signing in again is the only thing left to try.
    Ok(match &retried.provider_error {
        Some(error) if is_stale_oauth_token(error, resolved.source) => {
            let refused = refusal(error);
            login_lost_turn(
                retried,
                &provider_id,
                &refused,
                "the provider refused the renewed token too",
            )
        }
        _ => retried,
    })
}

/// Why a provider the operator picked cannot be used. Resolved again only to
/// say so: a login that has lapsed for good names itself and how to sign in
/// again, instead of a guess.
fn unavailable_provider(config: &arsy_kernel::config::Config, provider_id: &str) -> String {
    match provider::resolve(config, Some(provider_id)) {
        Err(refused) if !refused.remediation.is_empty() => format!(
            "provider `{provider_id}` is not available: {} — {}",
            refused.message, refused.remediation
        ),
        _ => format!(
            "provider `{provider_id}` is not available — its configuration or credential could \
             not be read; check it with /provider or /auth"
        ),
    }
}

/// Record what the stream returned that is not drawn: token usage, and
/// reasoning state kept for the round's history.
#[cfg(feature = "tui")]
fn absorb_quiet(outcome: &mut Turn, event: Streamed) {
    match event {
        Streamed::Usage {
            input_tokens,
            output_tokens,
        } => {
            outcome.usage = json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
        }
        Streamed::Reasoning(state) => outcome.reasoning.push(state),
        _ => {}
    }
}

/// A turn that failed because a provider's login is gone, said as such.
#[cfg(feature = "tui")]
fn login_lost_turn(mut outcome: Turn, provider_id: &str, refused: &str, cause: &str) -> Turn {
    let error = arsy_kernel::provider::ProviderError::Auth(provider::login_lost(
        provider_id,
        refused,
        cause,
    ));
    outcome.failure = Some(error.to_string());
    outcome.provider_error = Some(error);
    outcome
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
    footer: &Footer<'_>,
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
    let mut last_event = std::time::Instant::now();
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                status: &str| {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, &footer.row()));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, &status_line(false, 0))?;
    loop {
        let typed = drain_keys(keys, decoder, composer, approval, &mut outcome);
        if typed == Typed::Interrupted {
            // A half-written call never runs, so its card must not stay up
            // reading `writing` above the interruption.
            live.end_draft(&mut terminal, composer)?;
            draw(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                &status_line(first_event, tick),
            )?;
            return finish(terminal, composer, live.hand_over(outcome));
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
        match receive_watched(&events, wait, first_event, &mut last_event) {
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
            Ok(Ok(quiet @ (Streamed::Usage { .. } | Streamed::Reasoning(_)))) => {
                absorb_quiet(&mut outcome, quiet);
            }
            Ok(Ok(Streamed::ToolStarted { index, name })) => {
                let status = status_line(first_event, tick);
                live.tool_started(
                    &mut terminal,
                    composer,
                    colour,
                    footer,
                    &status,
                    index,
                    name,
                )?;
                first_event = true;
            }
            Ok(Ok(Streamed::ToolDelta { index, fragment })) => {
                let status = status_line(first_event, tick);
                live.tool_delta(
                    &mut terminal,
                    composer,
                    colour,
                    footer,
                    &status,
                    index,
                    &fragment,
                )?;
            }
            Ok(Ok(Streamed::Tool {
                id,
                name,
                arguments,
            })) => {
                live.end_draft(&mut terminal, composer)?;
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
                    return finish(terminal, composer, live.hand_over(outcome));
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
    finish(terminal, composer, live.hand_over(outcome))
}

/// One streamed fact from a provider, as the terminal needs it.
#[cfg(feature = "tui")]
enum Streamed {
    Text(String),
    Thinking(String),
    /// Reasoning state to keep with the round's history. Never drawn.
    Reasoning(Value),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A tool call has started; its arguments follow as fragments.
    ToolStarted {
        index: usize,
        name: String,
    },
    /// Raw argument bytes for display only. Never parsed into a call.
    ToolDelta {
        index: usize,
        fragment: String,
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

/// The Codex CLI sandbox a mode runs in.
///
/// The CLI answers its own tool calls, so none of them reaches `decide`, the
/// Auto safety review, or an approval card. Only Bypass — which asks for none
/// of those — may write; every other mode is held to read-only.
#[cfg(feature = "tui")]
fn codex_sandbox(mode: approval::ApprovalMode) -> &'static str {
    if mode == approval::ApprovalMode::BypassPermissions {
        "workspace-write"
    } else {
        "read-only"
    }
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
    footer: &Footer<'_>,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let mode = approval.get();
    let sandbox = codex_sandbox(mode);
    if sandbox == "read-only"
        && matches!(
            mode,
            approval::ApprovalMode::AcceptEdits | approval::ApprovalMode::Auto
        )
    {
        let mut terminal = io::stdout();
        writeln!(
            terminal,
            "{}",
            tui::tool_result_row(
                colour,
                "codex",
                false,
                &format!(
                    "{} cannot be enforced per action through the Codex CLI; this turn runs \
                     read-only",
                    mode.label()
                )
            )
        )?;
        terminal.flush()?;
    }
    let mut command = std::process::Command::new("codex");
    command.args([
        "exec",
        "--json",
        "--ephemeral",
        "--sandbox",
        sandbox,
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
    footer: &Footer<'_>,
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
    // One observation for both: the composer this sizes and the rows the
    // painter lays out belong to the same window.
    let (width, rows) = tui::terminal_dimensions();
    composer.set_height(rows);
    let painter = Painter {
        colour,
        footer,
        width: std::cell::Cell::new(width),
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
    /// What the model reasoned in the round this came from, as shown.
    pub(crate) thinking: Option<String>,
    /// Each round already recorded its thinking and text in the transcript,
    /// so the turn's answer is not recorded a second time.
    pub(crate) recorded: bool,
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
    /// Reasoning state the round's stream returned, kept ahead of its calls
    /// in the history so the model continues its own line of thought.
    pub(crate) reasoning: Vec<Value>,
    pub(crate) changed_files: std::collections::BTreeSet<String>,
    pub(crate) rules_granted: usize,
    /// What each compaction of the context during the turn recorded.
    pub(crate) compactions: Vec<Value>,
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
    } else if message.contains("tool worker disconnected") {
        (
            ARSY_TRN_1000,
            "turn",
            "a tool stopped before it answered; check the workspace and send the turn again",
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
    footer: &Footer<'_>,
    transcript: &mut tui::Transcript,
    completed_calls: &mut std::collections::HashMap<String, String>,
    interrupted: &mut bool,
) -> io::Result<(Vec<ModelContent>, bool, Vec<String>)> {
    let mut results = Vec::with_capacity(calls.len());
    let mut all_repeated = true;
    let mut changed = Vec::new();
    // Effects are not remembered across rounds, but the same effect sent twice
    // in one response is a provider stutter, and a commit must not run twice.
    let mut round_effects = std::collections::HashMap::<String, String>::new();
    for (id, name, arguments) in calls {
        let summary = runtime.summarize(name, arguments);
        let fingerprint = crate::loop_guard::fingerprint(name, arguments);
        let cached = completed_calls
            .get(&fingerprint)
            .or_else(|| round_effects.get(&fingerprint))
            .cloned();
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
                let result = run_call(
                    runtime,
                    intent_digest,
                    terminal,
                    colour,
                    &summary,
                    Call {
                        name,
                        arguments,
                        fingerprint: fingerprint.clone(),
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
                )?;
                if !result.is_error && !runtime.is_observational(name, arguments) {
                    round_effects.insert(fingerprint, result.output.clone());
                }
                result
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
                    Some(result.duration),
                );
            }
        }
        let card = if repeated {
            tui::tool_repeated_row(colour, name, &summary)
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
            "{}{}{}{}",
            block_gap(&card),
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

    /// Ctrl+T steps the effort off → low → medium → high → off, and a
    /// footer built before the change names the new one on its next draw.
    #[test]
    fn a_footer_follows_the_effort_and_mode_changed_mid_turn() {
        let approval = approval::ApprovalCell::default();
        let mut state = tui::TuiState::new("/tmp/w".to_owned(), SessionId::new());
        state.set_effort(None);
        let footer = Footer::live(&state, &approval, None, 120, false);
        assert!(footer.row().contains("off"), "{}", footer.row());

        assert_eq!(approval.cycle_effort(), Some(Effort::Low));
        assert_eq!(approval.cycle_effort(), Some(Effort::Medium));
        assert!(footer.row().contains("medium"), "{}", footer.row());

        cycle_approval_mode(&approval);
        assert!(
            footer.row().contains(approval.get().label()),
            "{}",
            footer.row()
        );

        approval.cycle_effort();
        assert_eq!(approval.cycle_effort(), None, "high wraps to off");
    }

    /// A compaction leaves a strip in the transcript that a repaint draws
    /// again, and says what `context.compacted` should record.
    #[test]
    fn a_compaction_is_shown_and_survives_a_repaint() {
        let trimmed = arsy_code::agent::budget::Trimmed {
            elided: 2,
            summarized: 5,
            before: 90_000,
            after: 40_000,
            summary: None,
        };
        let mut transcript = tui::Transcript::default();
        let mut composer = tui::Composer::default();
        let mut live = Vec::new();
        // While it runs, a live row names each step; the result replaces it
        // on the same line.
        let mut progress = CompactionProgress::new(90_000, Some(64_000));
        for step in [
            arsy_tui::widget::CompactionStep::Eliding,
            arsy_tui::widget::CompactionStep::Folding,
        ] {
            progress
                .show(&mut live, Some(&mut composer), false, step)
                .unwrap();
        }
        let detail = report_trim(
            &mut live,
            false,
            &trimmed,
            false,
            &mut transcript,
            &mut composer,
            &mut progress,
        )
        .unwrap()
        .expect("a change is reported");
        assert_eq!(detail["messages"], 5);
        assert_eq!(detail["trigger"], "budget");
        let live = String::from_utf8(live).unwrap();
        for step in ["1/4 measuring", "2/4 eliding", "3/4 folding"] {
            assert!(live.contains(step), "{step} missing from {live}");
        }
        let result = live.rfind("compacted 5 earlier messages").unwrap();
        let erased = live.rfind("\r\x1b[K").unwrap();
        assert!(
            live.rfind("3/4 folding").unwrap() < erased && erased < result,
            "the live row is erased before the result: {live:?}"
        );

        let mut screen = Vec::new();
        let state = tui::TuiState::new("/tmp/w".to_owned(), SessionId::new());
        transcript.repaint(&mut screen, 100, false, &state).unwrap();
        let screen = String::from_utf8(screen).unwrap();
        assert!(screen.contains("90000 tokens"), "{screen}");

        let unchanged = arsy_code::agent::budget::Trimmed::default();
        assert!(report_trim(
            &mut Vec::new(),
            false,
            &unchanged,
            false,
            &mut transcript,
            &mut composer,
            &mut CompactionProgress::new(0, None),
        )
        .unwrap()
        .is_none());
    }

    /// On a model listed once per effort, Ctrl+T steps through that model's
    /// own levels and each step names the variant the next request goes to.
    #[test]
    fn ctrl_t_on_a_variant_family_routes_to_its_variants() {
        let models: Vec<String> = [
            "gemini-3.1-pro-low",
            "gemini-3.1-pro-high",
            "claude-sonnet-4-6",
        ]
        .map(str::to_owned)
        .to_vec();
        let approval = approval::ApprovalCell::default();
        let profile = tui::family_profile(&tui::variant_levels(&models, "gemini-3.1-pro"));
        approval.set_effort_choices(profile.choices());
        approval.set_effort(profile.clamp(None));
        let mut sent = Vec::new();
        for _ in 0..3 {
            sent.push(tui::variant_for(
                &models,
                "gemini-3.1-pro",
                approval.effort(),
            ));
            approval.cycle_effort();
        }
        assert_eq!(
            sent,
            [
                "gemini-3.1-pro-high",
                "gemini-3.1-pro-low",
                "gemini-3.1-pro-high"
            ]
        );
        assert!(
            !live_effort("/effort off", &approval),
            "a family has no off"
        );
        assert!(
            !live_effort("/effort medium", &approval),
            "nor a level it lacks"
        );
        assert!(live_effort("/effort low", &approval));
    }

    /// `/effort high` typed while a turn runs changes the effort now; a line
    /// that is not a level is left to be queued.
    #[test]
    fn effort_typed_mid_turn_applies_at_once() {
        let approval = approval::ApprovalCell::default();
        assert!(live_effort("/effort high", &approval));
        assert_eq!(approval.effort(), Some(Effort::High));
        assert!(!live_effort("/effort loud", &approval));
        assert!(!live_effort("fix the tests", &approval));
        assert_eq!(approval.effort(), Some(Effort::High));
    }

    /// Only a refusal that names the reasoning knob, of a request that sent
    /// one, is retried without it.
    #[test]
    fn a_refused_effort_is_told_apart_from_other_bad_requests() {
        use arsy_kernel::provider::ProviderError;
        let request = |effort| CanonicalModelRequest {
            model: ModelKey {
                provider: "hari".to_owned(),
                model: "plain".to_owned(),
            },
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: 256,
            effort,
            idempotency_key: arsy_kernel::protocol::IdempotencyKey::new("t-0").unwrap(),
        };
        let refused = ProviderError::InvalidRequest(
            "Unsupported parameter: 'reasoning_effort' is not supported with this model."
                .to_owned(),
        );
        assert!(refused_effort(&request(Some(Effort::High)), &refused));
        assert!(!refused_effort(&request(None), &refused));
        assert!(!refused_effort(
            &request(Some(Effort::High)),
            &ProviderError::InvalidRequest("prompt is too long".to_owned())
        ));
        assert!(!refused_effort(
            &request(Some(Effort::High)),
            &ProviderError::Auth("thinking about it".to_owned())
        ));
    }

    /// Keys pressed while a tool call runs used to be thrown away. Shift+Tab
    /// and Ctrl+T now change the mode and effort, and a sent line is held
    /// for the turn to queue.
    #[test]
    fn keys_pressed_during_a_tool_call_are_kept() {
        let approval = approval::ApprovalCell::default();
        let mut composer = tui::Composer::default();
        let mut decoder = tui::Keys::default();
        let (sender, keys) = std::sync::mpsc::channel();
        for byte in b"\x1b[Z\x14next step\r" {
            sender.send(*byte).unwrap();
        }
        let before = approval.get();
        let cancelled = absorb_live_keys(
            &keys,
            &mut decoder,
            &mut composer,
            &approval,
            &mut io::stdout(),
            LiveCall {
                name: "shell",
                cancellable: false,
                operation_id: arsy_kernel::domain::OperationId::new(),
            },
            &mut 0,
            &mut false,
        )
        .unwrap();
        assert!(!cancelled);
        assert_ne!(approval.get(), before);
        assert_eq!(approval.effort(), Some(Effort::Low));
        assert_eq!(composer.take_held(), vec!["next step".to_owned()]);
    }

    /// Erasing a finished tool's card climbs past everything the composer drew
    /// above its input — the status and the queued follow-ups — before the
    /// card, so none of it is left behind in the scrollback.
    #[test]
    fn a_finished_card_is_erased_with_every_row_the_composer_drew_over_it() {
        tui::set_render_style(tui::RenderStyle::Classic);
        let mut composer = tui::Composer::default();
        composer.hold("lu analisa aja ya".to_owned());
        composer.hold("jgn edit".to_owned());
        let _ = composer.render_turn(80, false, "  ⠋ Working…", "  footer");

        let erase = erase_card(&mut composer, 2);
        // The gap, two queued rows, the status, and the pad: five up to the
        // top of the composer, then the card's two.
        assert!(erase.contains("\x1b[5A"), "{erase:?}");
        assert!(erase.ends_with("\x1b[2A\r\x1b[J"), "{erase:?}");
        assert_eq!(composer.held_len(), 2, "erasing the view keeps the queue");
    }

    /// The Codex CLI answers its own tool calls, so a mode that promises to
    /// ask or to review first cannot let it write.
    #[test]
    fn only_bypass_lets_the_codex_cli_write() {
        use approval::ApprovalMode::*;
        for mode in [Default, AcceptEdits, Plan, Auto, DontAsk] {
            assert_eq!(codex_sandbox(mode), "read-only", "{mode:?}");
        }
        assert_eq!(codex_sandbox(BypassPermissions), "workspace-write");
    }

    /// A long-running command's output is capped from the front. The cut is a
    /// byte count, and output with multi-byte characters put it inside one.
    #[test]
    fn capping_live_output_never_splits_a_character() {
        // One unit is 11 bytes, so eleven tail lengths put the cut at every
        // byte of it, including the middle of each multi-byte character.
        for tail in 0..11 {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut live = String::new();
            let newest = "b".repeat(tail);
            sender
                .send(format!("{}{newest}", "✓日🎉a".repeat(5_000)))
                .unwrap();
            absorb_output(&receiver, &mut live);
            assert!(live.len() <= 16_384, "{tail}: {}", live.len());
            assert!(live.ends_with(&newest), "the newest output is kept");
        }
    }

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
        live.answer(
            &mut screen,
            &mut composer,
            false,
            &Footer::fixed(""),
            "status",
            "สวัสดี",
        )
        .unwrap();
        assert!(!live
            .pace(
                &mut screen,
                &mut composer,
                false,
                &Footer::fixed(""),
                "status"
            )
            .unwrap());
        live.waiting_since = Some(std::time::Instant::now() - STALL);
        assert!(live
            .pace(
                &mut screen,
                &mut composer,
                false,
                &Footer::fixed(""),
                "status"
            )
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

    /// A table in the reasoning is held until it ends, then drawn as a grid
    /// in the thinking rows instead of as raw pipes.
    #[test]
    fn a_table_in_the_reasoning_is_drawn_as_a_grid() {
        let mut live = Streaming::default();
        let mut composer = tui::Composer::default();
        let mut screen: Vec<u8> = Vec::new();
        let footer = Footer::fixed("");
        for chunk in [
            "Comparing:\n| a | b |\n",
            "|---|---|\n| c | d |\n",
            "done\n",
        ] {
            live.reason(&mut screen, &mut composer, false, &footer, "status", chunk)
                .unwrap();
        }
        live.close_thinking(&mut screen, &mut composer, false, &footer, "status")
            .unwrap();
        let screen = String::from_utf8(screen).unwrap();
        assert!(screen.contains("┼"), "{screen}");
        assert!(!screen.contains("|---|"), "{screen}");
        assert!(screen.find("Comparing").unwrap() < screen.find("┼").unwrap());
        assert!(screen.find("┼").unwrap() < screen.find("done").unwrap());
    }

    #[test]
    fn a_split_never_lands_inside_a_table() {
        let (settled, rest, paragraph) =
            settle_split("Crates:\n| a | b |\n|---|---|\n| c | d |\n| e | f").unwrap();
        assert_eq!(settled, "Crates:\n");
        assert_eq!(rest, "| a | b |\n|---|---|\n| c | d |\n| e | f");
        assert!(!paragraph);
        assert!(settle_split("| a | b |\n|---|---|\n| c | d |\n").is_none());
    }

    fn field<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn complete_arguments_decode_every_string_field() {
        let pairs = partial_strings(r#"{"path":"a.rs","content":"fn main() {}\n","mode":7}"#);
        assert_eq!(field(&pairs, "path"), Some("a.rs"));
        assert_eq!(field(&pairs, "content"), Some("fn main() {}\n"));
        assert_eq!(field(&pairs, "mode"), None, "only strings are read");
    }

    #[test]
    fn a_value_still_arriving_is_read_as_far_as_it_goes() {
        let pairs = partial_strings(r#"{"path":"a.rs","content":"line one\nline tw"#);
        assert_eq!(field(&pairs, "content"), Some("line one\nline tw"));
    }

    #[test]
    fn an_escape_cut_off_is_left_out() {
        assert_eq!(
            field(&partial_strings(r#"{"content":"ab\"#), "content"),
            Some("ab")
        );
        assert_eq!(
            field(&partial_strings(r#"{"content":"ab\u00"#), "content"),
            Some("ab")
        );
        assert_eq!(
            field(&partial_strings(r#"{"content":"\u00e9\"q\""}"#), "content"),
            Some("é\"q\"")
        );
    }

    #[test]
    fn a_nested_value_and_a_key_inside_content_are_not_fields() {
        let pairs = partial_strings(
            r#"{"opts":{"path":"nested"},"content":"say \"path\": \"x\"","path":"real.rs"}"#,
        );
        assert_eq!(field(&pairs, "path"), Some("real.rs"));
        assert_eq!(field(&pairs, "content"), Some(r#"say "path": "x""#));
        assert!(partial_strings("not json").is_empty());
    }

    #[test]
    fn a_draft_card_grows_with_its_arguments_and_goes_when_the_call_completes() {
        let mut live = Streaming::default();
        let mut composer = tui::Composer::default();
        let mut screen: Vec<u8> = Vec::new();
        live.tool_started(
            &mut screen,
            &mut composer,
            false,
            &Footer::fixed(""),
            "status",
            0,
            "fs.write".to_owned(),
        )
        .unwrap();
        for fragment in [
            r#"{"path":"src/new.rs","#,
            r#""content":"fn a() {}\n"#,
            r#"fn b() {}"#,
        ] {
            if let Some(draft) = live.draft.as_mut() {
                draft.last_frame = None;
            }
            live.tool_delta(
                &mut screen,
                &mut composer,
                false,
                &Footer::fixed(""),
                "status",
                0,
                fragment,
            )
            .unwrap();
        }
        let drawn = String::from_utf8(screen.clone()).unwrap();
        assert!(
            drawn.contains("writing"),
            "the card says it is being written"
        );
        assert!(drawn.contains("fn b() {}"), "the newest content is shown");

        live.tool_delta(
            &mut screen,
            &mut composer,
            false,
            &Footer::fixed(""),
            "status",
            1,
            "ignored",
        )
        .unwrap();
        live.end_draft(&mut screen, &mut composer).unwrap();
        assert!(live.draft.is_none(), "the draft is taken down");
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
        live.answer(
            &mut screen,
            &mut composer,
            false,
            &Footer::fixed(""),
            "status",
            &answer,
        )
        .unwrap();
        while live.has_pending() {
            live.last_frame = None;
            live.pace(
                &mut screen,
                &mut composer,
                false,
                &Footer::fixed(""),
                "status",
            )
            .unwrap();
            assert!(
                live.live_lines <= tui::terminal_rows(),
                "{} live rows cannot all be erased",
                live.live_lines
            );
        }
        let before_close = screen.len();
        live.close(
            &mut screen,
            &mut composer,
            false,
            &Footer::fixed(""),
            "status",
        )
        .unwrap();

        // What `close` settles is the tail of an answer whose head is already
        // in scrollback, so it carries no second marker.
        let tail = String::from_utf8(screen[before_close..].to_vec()).unwrap();
        assert!(live.continued, "the head settled while it streamed");
        assert!(!tail.contains('✦'), "one marker per answer:\n{tail}");
        assert!(tail.contains("paragraph 79"), "the last words are drawn");
    }

    /// A turn stopped after one round of tool calls: the task, the calls, and
    /// their results, as `native_turn` leaves them.
    fn stopped_after_a_round() -> Vec<ModelMessage> {
        vec![
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "fix the cart total".to_owned(),
                }],
            },
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::ToolCall {
                    id: "c1".to_owned(),
                    name: "fs.edit".to_owned(),
                    arguments: serde_json::json!({"path": "shop/cart.py"}),
                }],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::ToolResult {
                    id: "c1".to_owned(),
                    content: "edited shop/cart.py".to_owned(),
                    is_error: false,
                }],
            },
        ]
    }

    fn closing_note(conversation: &[ModelMessage]) -> &str {
        let last = conversation.last().unwrap();
        assert_eq!(last.role, ModelRole::Assistant);
        match last.content.last().unwrap() {
            ModelContent::Text { text } => text,
            other => panic!("the turn closes with text, not {other:?}"),
        }
    }

    #[test]
    fn an_interrupted_turn_stays_in_the_conversation_with_what_it_did() {
        let mut conversation = stopped_after_a_round();
        let turn = Turn {
            interrupted: true,
            ..Turn::default()
        };
        settle_stopped_turn(&mut conversation, 0, &turn);

        assert_eq!(
            conversation.len(),
            4,
            "the task, the call, and its result are kept"
        );
        assert!(matches!(
            &conversation[1].content[0],
            ModelContent::ToolCall { name, .. } if name == "fs.edit"
        ));
        assert!(closing_note(&conversation).contains("operator interrupted this turn"));
    }

    #[test]
    fn an_answer_interrupted_mid_stream_keeps_its_words() {
        let mut conversation = stopped_after_a_round();
        conversation.truncate(1);
        let turn = Turn {
            interrupted: true,
            response: "The bug is in Cart.total: it ignores".to_owned(),
            ..Turn::default()
        };
        settle_stopped_turn(&mut conversation, 0, &turn);

        let note = closing_note(&conversation);
        assert!(
            note.starts_with("The bug is in Cart.total: it ignores\n\n["),
            "{note}"
        );
        assert!(note.contains("interrupted"));
    }

    #[test]
    fn an_outage_keeps_the_work_done_before_it() {
        let mut conversation = stopped_after_a_round();
        let error = arsy_kernel::provider::ProviderError::Server {
            status: 503,
            message: "overloaded".to_owned(),
        };
        let turn = Turn {
            failure: Some(error.to_string()),
            provider_error: Some(error),
            ..Turn::default()
        };
        settle_stopped_turn(&mut conversation, 0, &turn);

        assert_eq!(conversation.len(), 4);
        let note = closing_note(&conversation);
        assert!(
            note.contains("stopped here") && note.contains("overloaded"),
            "{note}"
        );
    }

    #[test]
    fn a_rejected_request_is_rewound_so_it_cannot_fail_every_later_turn() {
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "an earlier turn".to_owned(),
            }],
        }];
        let base = conversation.len();
        conversation.extend(stopped_after_a_round());
        let error = arsy_kernel::provider::ProviderError::InvalidRequest(
            "missing thought_signature".to_owned(),
        );
        let turn = Turn {
            failure: Some(error.to_string()),
            provider_error: Some(error),
            ..Turn::default()
        };
        settle_stopped_turn(&mut conversation, base, &turn);

        assert_eq!(conversation.len(), base, "the turn that sent it is rewound");
    }

    #[test]
    fn a_completed_turn_is_left_to_its_own_path() {
        let mut conversation = stopped_after_a_round();
        settle_stopped_turn(&mut conversation, 0, &Turn::default());
        assert_eq!(conversation, stopped_after_a_round());
    }
}
