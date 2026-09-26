//! `arsy run` and `arsy resume`: one scripted task from prompt to recorded
//! outcome, whichever state the session store was left in.

use crate::*;

/// Run one scripted turn for `arsy run`.
#[cfg(feature = "tui")]
pub(crate) fn run(
    invocation: &Invocation,
    task: &str,
    image: Option<&Path>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let task = if task == "-" {
        read_stdin()?
    } else {
        task.to_owned()
    };
    let goal = prepare_task(&task, emitter)?;
    // Read before the session is opened: a path that is not an image, or is
    // too large, is the operator's mistake and should not cost a recorded turn.
    let attached = image.map(read_image).transpose()?;
    let mut execution = match TaskRun::open(invocation, None) {
        Ok(execution) => execution,
        Err(diagnostic) => return Ok(unusable(diagnostic, emitter)),
    };
    if let Some(note) = &execution.insight.note {
        emitter.diagnostic(&crate::probelm::unavailable(note));
    }
    // A warning, never a refusal: what probelm says is untrusted and may not
    // decide what a turn is allowed to send.
    if attached.is_some() && execution.insight.accepts_images(&execution.model) == Some(false) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-PRB-1001",
            format!(
                "probelm reports that `{}` does not accept images; the attached image may be refused",
                execution.model
            ),
            "pick a vision model with --model, or drop --image",
        ));
    }
    emitter.session = Some(execution.session);
    let task = execution.enqueue(&goal)?;
    execution.attached = attached;
    execution.execute(task, Value::Null, emitter)
}

/// The most one attached image may be.
///
/// Large enough for a full-resolution screenshot, small enough that a
/// mis-typed path to a video does not become a request nobody can send. The
/// encoding grows it by a third, and every endpoint in this family refuses
/// well below that.
pub(crate) const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Read an image the operator attached, as canonical model content.
pub(crate) fn read_image(path: &Path) -> Result<ModelContent, Diagnostic> {
    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        other => {
            return Err(usage(format!(
                "--image accepts png, jpeg, gif, or webp, not `{}`",
                other.unwrap_or("a file with no extension")
            )))
        }
    };
    let size = std::fs::metadata(path)
        .map_err(|error| usage(format!("--image {}: {error}", path.display())))?
        .len();
    if size > MAX_IMAGE_BYTES {
        return Err(usage(format!(
            "--image {} is {size} bytes; the limit is {MAX_IMAGE_BYTES}",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| usage(format!("--image {}: {error}", path.display())))?;
    Ok(ModelContent::Image {
        media_type: media_type.to_owned(),
        data: arsy_kernel::provider::base64(&bytes),
    })
}

/// A misconfiguration is the operator's to fix, not a failed turn in their
/// session history, so nothing is recorded before it is reported.
pub(crate) fn unusable(mut diagnostic: Diagnostic, emitter: &mut Emitter) -> i32 {
    if diagnostic.code == ARSY_PRV_1000 {
        diagnostic.remediation = format!(
            "{}; or use the interactive TUI with a logged-in Codex CLI",
            diagnostic.remediation
        );
    }
    emitter.diagnostic(&diagnostic);
    diagnostic.exit_code()
}

/// How long a task's lease runs before another process may take it over.
///
/// A turn still in flight has not lost its lease; a process that died halfway
pub(crate) const TASK_LEASE_MS: u64 = 30 * 60 * 1000;

/// What one task may spend before it is stopped rather than continued.
///
/// Wall time matches the lease, because a task that outlives its lease is one
/// another process may already have taken. Tokens are several turns' worth of
/// transcript: the point is to stop a runaway, not to second-guess a long task.
pub(crate) const TASK_BUDGET: Budget = Budget {
    tokens: CONTEXT_BUDGET_TOKENS as u64 * 4,
    cost_micros: u64::MAX,
    wall_ms: TASK_LEASE_MS,
};

/// One session's execution: the store, the provider it dispatches to, and the
/// durable graph of tasks it is working through.
///
/// `arsy run` and `arsy resume` differ only in where the task comes from — a
/// new one, or one a dead process left behind — so everything after that point
/// is this, and a resumed task cannot drift from a fresh one by being executed
/// somewhere else.
pub(crate) struct TaskRun {
    pub(crate) root: PathBuf,
    pub(crate) config: Config,
    pub(crate) resolved: provider::Resolved,
    pub(crate) model: String,
    pub(crate) service: AgentService,
    pub(crate) actor: Principal,
    pub(crate) session: SessionId,
    pub(crate) graph: TaskGraph,
    /// This process's identity as a task holder, so an expired lease can be
    /// told from one this process still holds.
    pub(crate) agent: AgentId,
    /// An image `--image` attached to the prompt, sent with the first message.
    pub(crate) attached: Option<ModelContent>,
    /// What probelm reported about the configured models when this run opened.
    pub(crate) insight: crate::probelm::ModelInsight,
}

impl TaskRun {
    /// Resolve everything a turn needs, then attach to the session.
    ///
    /// Configuration is resolved first and on its own: a provider that cannot
    /// be reached is a diagnostic before anything is recorded.
    pub(crate) fn open(
        invocation: &Invocation,
        session: Option<SessionId>,
    ) -> Result<Self, Diagnostic> {
        let root = workspace_root(&invocation.workspace)?;
        let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
        let config = load_config(&root, &working, invocation.config.as_deref())?;
        // Before routing, so the first turn's route already has what probelm
        // knows rather than waiting for a snapshot that does not exist yet.
        let insight = crate::probelm::gather(&config, true);
        let mut resolved = provider::resolve(&config, invocation.provider.as_deref(), &insight)?;
        let model = selected_model(&config, &resolved.endpoint, invocation.model.as_deref())?;
        resolved.context_window = insight.context_window(&model);

        let store = open_store(&root)?;
        let session = session.unwrap_or_default();
        let actor = actor();
        let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
            .map_err(storage_failed)?;
        for change in &insight.transitions {
            service
                .record_health_change(actor.clone(), change)
                .map_err(storage_failed)?;
        }
        let graph = TaskGraph::new(store, session, actor.clone()).map_err(graph_failed)?;
        Ok(Self {
            root,
            config,
            resolved,
            model,
            service,
            actor,
            session,
            graph,
            agent: AgentId::new(),
            attached: None,
            insight,
        })
    }

    /// Record a new task in the graph and take it.
    pub(crate) fn enqueue(&mut self, goal: &str) -> Result<TaskId, Diagnostic> {
        let id = TaskId::new();
        self.graph
            .add(TaskNode {
                id,
                goal: goal.to_owned(),
                dependencies: Vec::new(),
                assignee: Some(self.agent),
                required_output: "an answer to the task".to_owned(),
                // One process, one working tree: a scripted run edits the
                // workspace it was pointed at.
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget: TASK_BUDGET,
                // Authority comes from policy at dispatch, not from the node:
                // a grant recorded here would be a second, stale answer to the
                // question `RuleSet::evaluate` already answers per call.
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: Default::default(),
            })
            .map_err(graph_failed)?;
        Ok(id)
    }

    /// Run one task to a terminal state, recording what it spent on the way.
    ///
    /// `context` is folded into the result: a resumed task reports what its
    /// recovery found in the same record as its outcome, so one invocation
    /// still produces exactly one result.
    pub(crate) fn execute(
        &mut self,
        task: TaskId,
        context: Value,
        emitter: &mut Emitter,
    ) -> Result<i32, Diagnostic> {
        self.graph.ready().map_err(graph_failed)?;
        self.graph
            .lease(task, self.agent, unix_time_ms() + TASK_LEASE_MS)
            .map_err(graph_failed)?;
        let goal = self
            .graph
            .node(task)
            .map(|node| node.goal.clone())
            .ok_or_else(|| storage_failed("the task disappeared from its own graph"))?;

        // No operator is present, so nothing can be confirmed mid-run: the risk
        // context says so, and a call that needs an approval is refused by
        // policy rather than waiting on a keyboard that is not there.
        let agent = agent_runtime(
            &self.root,
            &self.config,
            false,
            &task.to_string(),
            Some(self.session),
            Some((
                task,
                self.graph
                    .node(task)
                    .and_then(|node| node.runtime.current_attempt),
            )),
            None,
            emitter,
            // A subagent works from the same prompt its parent was given, so
            // it reads the same skill listing.
            &prompt_skills(&self.root, &self.config),
        )?;
        // A supervisor exists only when policy actually delegates something,
        // so a workspace that grants nothing sees no spawn tool rather than one
        // that always refuses.
        let supervisor = subagent::Supervisor::new(
            self.root.clone(),
            &self.config,
            &self.resolved,
            self.model.clone(),
            task,
            &agent,
        );
        let delegates = supervisor.can_delegate();
        // Loaded once, before the turn starts: a turn finishes with the hooks
        // it began with, so a file edited mid-run cannot change the rules under
        // an agent already applying them.
        let loaded = hook_engine(&self.root, &self.config);
        let hooks = (!loaded.is_empty()).then_some(&loaded.engine);
        let goal = match turn_boundary(
            hooks,
            arsy_code::hook::LifecycleEvent::BeforeTurn,
            &goal,
            emitter,
        ) {
            Ok(goal) => goal,
            Err(reason) => {
                return Err(Diagnostic::error(
                    "ARSY-HOK-1001",
                    reason,
                    "the hook that refused it is listed by `arsy hook list`",
                ))
            }
        };
        let admission = self.start_turn(&goal)?;
        let request = CanonicalModelRequest {
            model: ModelKey {
                provider: self.resolved.endpoint.id.clone(),
                model: self.model.clone(),
            },
            system: system_prompt(
                &self.root,
                &self.config,
                &self.resolved.endpoint.id,
                &self.model,
                arsy_code::agent::ExecutionMode::Normal,
            ),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: std::iter::once(ModelContent::Text { text: goal })
                    .chain(self.attached.clone())
                    .collect(),
            }],
            tools: {
                let mut tools = agent.schemas();
                tools.extend(subagent::schemas(delegates));
                tools
            },
            max_output_tokens: self.resolved.endpoint.max_output_tokens,
            // Reasoning effort is chosen in the TUI with `/effort`. A scripted
            // run takes the request it always took, so a remembered interactive
            // choice cannot quietly change what a pipeline sends.
            effort: None,
            // The turn id, so a retried attempt is provably the same request.
            idempotency_key: IdempotencyKey::new(admission.turn.to_string())
                .map_err(|error| storage_failed(error.to_string()))?,
        };

        let started = Instant::now();
        let mut recorder = telemetry::Recorder::new(&self.config, self.actor.clone())?;
        let max_parallel_tools = self.config.max_parallel_tools();
        let requested_provider = self.resolved.endpoint.id.clone();
        let (outcome, interventions) = dispatch_with_refresh(
            &mut self.resolved,
            &self.config,
            Some(&requested_provider),
            &self.root,
            &self.model,
            task,
            &agent,
            &request,
            &mut recorder,
            &mut self.graph,
            hooks,
            max_parallel_tools,
            emitter,
        );
        let stop = match &outcome {
            Ok(_) => "answered".to_owned(),
            Err(error) => format!("provider:{}", error.code()),
        };
        // The turn has ended whatever it ended as, which is what Codex's own
        // `notify` is for. Nothing it returns can change what already happened.
        let _ = turn_boundary(
            hooks,
            arsy_code::hook::LifecycleEvent::AfterTurn,
            &stop,
            emitter,
        );
        let summary = recorder.finish(&stop, &redactor(emitter)?, emitter);
        let mut record = json!({
            "session": self.session.to_string(),
            "task": task.to_string(),
            "turn": admission.turn.to_string(),
            "provider": self.resolved.endpoint.id,
            "model": request.model.model,
            "telemetry": summary,
            "interventions": interventions,
            // The same projection `/plan` and `/todo` draw, so a pipeline can
            // read where the work stands without replaying raw events — and
            // sees exactly what an operator watching the TUI would have seen.
            "plan": progress::plan(&self.root, &task.to_string()),
            "todos": open_store(&self.root)
                .ok()
                .and_then(|store| progress::todos(store as Arc<dyn EventStore>, self.session)),
        });
        merge(&mut record, context);
        let input_tokens = summary_number(&record, "input_tokens");
        let output_tokens = summary_number(&record, "output_tokens");
        // What the turn cost, when configuration says what the model charges.
        // `None` is reported as unknown rather than as zero: a session that
        // claims it spent nothing is worse than one that admits it cannot say.
        let priced = charge(
            Some(&self.resolved.endpoint),
            &self.model,
            input_tokens,
            output_tokens,
        );
        merge(
            &mut record,
            json!({
                "cost_micros": priced,
                "cost_source": if priced.is_some() { "configured" } else { "unknown" },
            }),
        );
        // Recorded whether the turn completed or failed: a turn that died
        // halfway still spent the tokens it spent, and a session's totals are
        // wrong if the failures are missing from them.
        self.service
            .record_usage(
                self.actor.clone(),
                arsy_kernel::projection::UsageTotals {
                    input_tokens,
                    output_tokens,
                    cost_micros: priced,
                },
            )
            .map_err(storage_failed)?;
        // Charged before the task is closed, so an exhausted budget is on the
        // record even when the turn it exhausted answered anyway.
        if let Err(error) = self.graph.consume(
            task,
            Budget {
                tokens: input_tokens + output_tokens,
                cost_micros: priced.unwrap_or(0),
                wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        ) {
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-RET-1000",
                error.to_string(),
                "split the task, or raise what one task may spend",
            ));
        }

        match outcome {
            Ok(usage) => {
                let mut outcome = record.clone();
                merge(&mut outcome, usage);
                self.service
                    .record_transcript(
                        self.actor.clone(),
                        admission.turn,
                        outcome.get("transcript").unwrap_or(&Value::Null),
                    )
                    .map_err(storage_failed)?;
                self.service
                    .complete_turn(self.actor.clone(), admission.turn, &outcome)
                    .map_err(storage_failed)?;
                self.graph
                    .complete(task, outcome.clone())
                    .map_err(graph_failed)?;
                let mut result = outcome;
                merge(&mut result, json!({"status": "completed"}));
                emitter.result(result);
                Ok(0)
            }
            Err(error) => {
                let diagnostic = Diagnostic::error(
                    ARSY_PRV_1000,
                    error.to_string(),
                    "check the provider endpoint, credential, and model in `arsy config explain`",
                );
                // The turn is durable before dispatch, so a failure here stays
                // recoverable through `arsy resume`.
                self.service
                    .fail_turn(
                        self.actor.clone(),
                        admission.turn,
                        error.code(),
                        error.to_string(),
                    )
                    .map_err(storage_failed)?;
                self.graph
                    .fail(
                        task,
                        json!({"code": error.code(), "message": error.to_string()}),
                    )
                    .map_err(graph_failed)?;
                emitter.diagnostic(&diagnostic);
                let mut result = record;
                merge(&mut result, json!({"status": "failed"}));
                emitter.result(result);
                Ok(diagnostic.exit_code())
            }
        }
    }

    pub(crate) fn start_turn(
        &self,
        goal: &str,
    ) -> Result<arsy_kernel::service::TurnAdmission, Diagnostic> {
        let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session: self.session,
            prompt: goal.to_owned(),
            extensions: Extensions::new(),
        }));
        self.service
            .start_turn(self.actor.clone(), &envelope)
            .map_err(storage_failed)
    }
}

pub(crate) fn graph_failed(error: arsy_kernel::orchestration::GraphError) -> Diagnostic {
    Diagnostic::error(
        "ARSY-STL-1000",
        format!("the task graph refused the change: {error}"),
        "inspect the session with `arsy session show --turns`",
    )
}

/// The transcript budget, in tokens, before the model's own output is reserved.
///
/// Deliberately below the smallest window the supported models offer rather
/// than read from configuration: the cost of being wrong low is a re-read, and
/// the cost of being wrong high is a rejected request in the middle of a turn.
/// A per-model window belongs in `provider.endpoint` when a model that needs a
/// different number actually appears.
pub(crate) const CONTEXT_BUDGET_TOKENS: u32 = 96_000;

/// What one turn's transcript may grow to on this endpoint.
///
/// A window probelm reported can only shrink the cap, never raise it, so no
/// model's cost envelope grows because an untrusted source said it could.
pub(crate) fn context_budget(resolved: &provider::Resolved) -> u32 {
    let window = resolved
        .context_window
        .map_or(CONTEXT_BUDGET_TOKENS, |window| {
            u32::try_from(window)
                .unwrap_or(u32::MAX)
                .min(CONTEXT_BUDGET_TOKENS)
        });
    window.saturating_sub(resolved.endpoint.max_output_tokens)
}

/// Dispatch a scripted turn, and once more if a stale OAuth access token is
/// why it failed.
///
/// `dispatch` itself never retries a [`ProviderError::Auth`]: an identical
/// request would fail identically, the way `stream_with_retry`'s own doc
/// comment says. What is retryable here is not the request but the
/// credential — `arsy run` resolves a provider once and keeps it for the
/// whole task (see `TaskRun::open`), so a token that expires mid-task is
/// never re-checked until this catches it. Re-resolving the same endpoint
/// exercises the refresh path `provider::stored` already has; a plain API
/// key is left alone; a refresh that itself fails surfaces the original
/// error unchanged, asking the operator to sign in again.
///
/// A retry rebuilds the delegation supervisor rather than reusing the
/// first one, which is safe: an auth failure happens on the very first
/// model call, before any delegation this task's supervisor could lose.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_with_refresh(
    resolved: &mut provider::Resolved,
    config: &Config,
    requested_provider: Option<&str>,
    root: &Path,
    model: &str,
    task: TaskId,
    agent: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    recorder: &mut telemetry::Recorder,
    graph: &mut TaskGraph,
    hooks: Option<&arsy_code::hook::HookEngine>,
    max_parallel_tools: usize,
    emitter: &mut Emitter,
) -> (Result<Value, ProviderError>, Vec<Value>) {
    let supervisor = subagent::Supervisor::new(
        root.to_path_buf(),
        config,
        resolved,
        model.to_owned(),
        task,
        agent,
    );
    // Always present: a supervisor also owns `task.criterion`, and a turn
    // that cannot delegate still has to be able to commit to what done means.
    let mut supervising = Some((supervisor, &mut *graph));
    let outcome = dispatch(
        config,
        context_budget(resolved),
        resolved.provider.as_ref(),
        agent,
        request,
        recorder,
        &mut supervising,
        hooks,
        max_parallel_tools,
        emitter,
    );
    // The turn is over, so nothing is left to read a child's answer: stop and
    // reap them here rather than leaving threads spending a budget nobody is
    // waiting on.
    if let Some((supervisor, graph)) = supervising.as_mut() {
        supervisor.finish(emitter);
        // After the children are settled and before anything reports the
        // turn: whether the work meets what it committed to is decided from
        // the record, not from how the turn ended.
        supervisor.settle_proof(graph, emitter);
    }
    let interventions: Vec<Value> = supervising
        .as_ref()
        .map(|(supervisor, _)| supervisor.interventions().to_vec())
        .unwrap_or_default();
    drop(supervising);

    let stale = matches!(&outcome, Err(error) if is_stale_oauth_token(error, resolved.source));
    if !stale {
        return (outcome, interventions);
    }
    let Ok(mut refreshed) = provider::resolve(
        config,
        requested_provider,
        &crate::probelm::ModelInsight::default(),
    ) else {
        return (outcome, interventions);
    };
    // The same endpoint and model, so the window it was budgeted for holds.
    refreshed.context_window = resolved.context_window;
    *resolved = refreshed;
    emitter.trace(
        "credential.refreshed",
        json!({"provider": resolved.endpoint.id}),
    );
    let supervisor = subagent::Supervisor::new(
        root.to_path_buf(),
        config,
        resolved,
        model.to_owned(),
        task,
        agent,
    );
    let mut supervising = Some((supervisor, graph));
    let outcome = dispatch(
        config,
        context_budget(resolved),
        resolved.provider.as_ref(),
        agent,
        request,
        recorder,
        &mut supervising,
        hooks,
        max_parallel_tools,
        emitter,
    );
    if let Some((supervisor, graph)) = supervising.as_mut() {
        supervisor.finish(emitter);
        // After the children are settled and before anything reports the
        // turn: whether the work meets what it committed to is decided from
        // the record, not from how the turn ended.
        supervisor.settle_proof(graph, emitter);
    }
    let interventions = supervising
        .as_ref()
        .map(|(supervisor, _)| supervisor.interventions().to_vec())
        .unwrap_or_default();
    (outcome, interventions)
}

/// Whether a failure is worth resolving a fresh credential and trying
/// again for: only an authentication failure, and only when the
/// credential came from an OAuth login. An API key that is rejected will
/// be rejected identically the second time, and any other error class is
/// already `stream_with_retry`'s job, not this one's.
pub(crate) fn is_stale_oauth_token(
    error: &ProviderError,
    source: provider::CredentialSource,
) -> bool {
    matches!(error, ProviderError::Auth(_)) && source == provider::CredentialSource::OAuth
}

/// Run one scripted turn to completion, executing the tools the model asks for.
///
/// Nobody is at the keyboard, so authority comes from policy alone: a call
/// policy allows runs, and a call that needs an approval is reported to the
/// model as a failed result rather than silently skipped. That is what makes a
/// pipeline's behaviour a property of its configuration instead of a property
/// of who happened to be watching.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch(
    config: &Config,
    budget: u32,
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    recorder: &mut telemetry::Recorder,
    supervisor: &mut Option<(subagent::Supervisor<'_>, &mut TaskGraph)>,
    hooks: Option<&arsy_code::hook::HookEngine>,
    parallel: usize,
    emitter: &mut Emitter,
) -> Result<Value, ProviderError> {
    let max_rounds = config.max_tool_rounds();
    let mut request = request.clone();
    let base = request.idempotency_key.as_str().to_owned();
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    for round in 0..max_rounds {
        // Each round is its own request, so a retry repeats that round rather
        // than collapsing into the one before it.
        request.idempotency_key = IdempotencyKey::new(format!("{base}-{round}"))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        emitter.trace(
            "request",
            json!({
                "round": round,
                "provider": request.model.provider,
                "model": request.model.model,
                "idempotency_key": request.idempotency_key.as_str(),
                "messages": request.messages.len(),
                "tools": request.tools.len(),
                "max_output_tokens": request.max_output_tokens,
                "context_budget_tokens": budget,
            }),
        );
        let mut answer = String::new();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        // Each sleep the retry loop asks for is one attempt that failed, which
        // is the only place a retry is observable from outside the provider.
        let mut retries = 0;
        let started = Instant::now();
        let stream = arsy_kernel::provider::stream_with_retry(provider, &request, &mut |delay| {
            retries += 1;
            // The only place a retry is observable from outside the provider,
            // and the question an operator debugging a slow turn is asking.
            emitter.trace(
                "retry",
                json!({"round": round, "attempt": retries, "delay_ms": delay.as_millis() as u64}),
            );
            std::thread::sleep(delay);
        })
        .inspect_err(|error| {
            recorder.model_call(
                &request.model.model,
                started.elapsed(),
                0,
                0,
                retries,
                error.code(),
            );
        })?;
        let (mut round_input, mut round_output) = (0u64, 0u64);
        let round_calls = absorb_stream_events(
            stream,
            emitter,
            &mut answer,
            &mut input_tokens,
            &mut output_tokens,
            &mut round_input,
            &mut round_output,
            round,
        )?;
        calls.extend(round_calls);
        recorder.model_call(
            &request.model.model,
            started.elapsed(),
            round_input,
            round_output,
            retries,
            "ok",
        );
        emitter.trace(
            "round.finished",
            json!({
                "round": round,
                "input_tokens": round_input,
                "output_tokens": round_output,
                "retries": retries,
                "tool_calls": calls.len(),
                "answer_bytes": answer.len(),
            }),
        );
        if calls.is_empty() {
            emitter.end_deltas();
            if !answer.trim().is_empty() {
                request.messages.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::Text {
                        text: answer.clone(),
                    }],
                });
            }
            let mut usage = token_usage(input_tokens, output_tokens);
            merge(
                &mut usage,
                json!({
                    "response": answer,
                    // Everything after the system prompt's own message: the
                    // turn's exchange, which is what a resume replays.
                    "transcript": transcript::persistable(&request.messages),
                }),
            );
            return Ok(usage);
        }
        arsy_code::agent::budget::fit(&mut request.messages, budget, None);

        // The calls are history now, whatever running them produced: a provider
        // that sent a call and never sees its result rejects the next request.
        let mut content: Vec<ModelContent> = Vec::new();
        if !answer.trim().is_empty() {
            content.push(ModelContent::Text { text: answer });
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
        request.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });
        // Independent reads run together; anything that writes, runs a
        // command, or spawns a child still runs alone and in order. `hooks`
        // are the exception: a hook engine is one interpreter with its own
        // recursion guard, so a workspace that loads hooks keeps the
        // sequential path rather than racing them.
        let batched = if hooks.is_none()
            && parallel > 1
            && !calls
                .iter()
                .any(|(_, name, _)| subagent::OWNED_TOOLS.contains(&name.as_str()))
        {
            let batch: Vec<(String, Value)> = calls
                .iter()
                .map(|(_, name, arguments)| (name.clone(), arguments.clone()))
                .collect();
            runtime.invoke_batch(&batch, parallel)
        } else {
            Vec::new()
        };
        let results = calls
            .iter()
            .enumerate()
            .map(|(position, (id, name, arguments))| {
                // Spawning is the one call the tool runtime does not own: it
                // adds a node to this session's graph rather than touching the
                // workspace, and the child's own calls go through the runtime
                // under the authority the graph attenuated for it.
                let result = match (batched.get(position), name.as_str(), supervisor.as_mut()) {
                    // Already run, in whatever order the batch chose; the
                    // position is what pairs it back to this call's id.
                    (Some(result), _, _) => result.clone(),
                    (None, name, Some((supervisor, graph)))
                        if subagent::OWNED_TOOLS.contains(&name) =>
                    {
                        supervisor.call(name, arguments, graph, emitter)
                    }
                    _ => invoke_hooked(hooks, runtime, name, arguments, emitter),
                };
                recorder.tool_call(&result);
                emitter.trace(
                    "tool.result",
                    json!({
                        "round": round,
                        "id": id,
                        "name": name,
                        "success": result.success,
                        "duration_ms": result.duration.as_millis() as u64,
                        "output_bytes": result.output.len(),
                        "changed_files": result.changed_files,
                        "artifact": result.artifact.map(|id| id.to_string()),
                    }),
                );
                ModelContent::ToolResult {
                    id: id.clone(),
                    content: result.output,
                    is_error: !result.success,
                }
            })
            .collect();
        request.messages.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
    }
    emitter.end_deltas();
    Err(ProviderError::InvalidRequest(format!(
        "the model asked for tools {max_rounds} times without finishing the turn — the budget is \
         `execution.max_tool_rounds`; raise it, or continue with a narrower task"
    )))
}

/// Where Claude Code and Codex keep the operator's files.
///
/// A unit test gets none at all, so what it asserts cannot depend on the
pub(crate) fn charge_turn(
    resolved: Option<&provider::Resolved>,
    model: &str,
    usage: &Value,
) -> Option<u64> {
    charge(
        resolved.map(|resolved| &resolved.endpoint),
        model,
        summary_number(usage, "input_tokens"),
        summary_number(usage, "output_tokens"),
    )
}

/// As above, from the parts. A turn that spent no tokens cost nothing at any
/// price, so it is known to be free rather than unknown — otherwise a failed
/// turn would make a whole session's total unknowable.
pub(crate) fn charge(
    endpoint: Option<&arsy_kernel::config::Endpoint>,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<u64> {
    if input_tokens == 0 && output_tokens == 0 {
        return Some(0);
    }
    Some(
        endpoint?
            .pricing
            .get(model)?
            .cost_micros(input_tokens, output_tokens),
    )
}

pub(crate) fn token_usage(input_tokens: u64, output_tokens: u64) -> Value {
    if input_tokens == 0 && output_tokens == 0 {
        json!({})
    } else {
        json!({"input_tokens": input_tokens, "output_tokens": output_tokens})
    }
}

/// Fold `extra`'s fields into `target`, which is always an object here.
pub(crate) fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) fn prepare_task(task: &str, emitter: &mut Emitter) -> Result<String, Diagnostic> {
    if task.trim().is_empty() {
        return Err(usage("run requires a non-empty task"));
    }
    redactor(emitter)?.sanitize(task).map_err(secret_failed)
}

pub(crate) fn resume(
    invocation: &Invocation,
    session: SessionId,
    follow: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    if store
        .current_version(session)
        .map_err(|error| storage_failed(error.to_string()))?
        .0
        == 0
    {
        return Err(Diagnostic::error(
            "ARSY-SCH-1004",
            format!("session {session} has no recorded events in this workspace"),
            "check the ID, or run `arsy run` with the workspace that recorded it",
        ));
    }

    // Recovery needs no provider: closing what a dead process left open is
    // worth doing even in a workspace that could not dispatch a turn today.
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
        .map_err(storage_failed)?;
    // History is never truncated: a turn that was running when the process
    // died is closed by appending `turn.failed` after the events it already
    // wrote.
    let interrupted = service.unfinished_turns().map_err(storage_failed)?;
    for turn in &interrupted {
        service
            .fail_turn(
                actor.clone(),
                *turn,
                "interrupted",
                "the process exited before the turn finished",
            )
            .map_err(storage_failed)?;
    }
    let version = service.committed_version().map_err(storage_failed)?;
    drop(service);

    // A task whose lease has run out is one nobody is working on, whatever the
    // process that took it intended.
    let mut graph = TaskGraph::new(store, session, actor).map_err(graph_failed)?;
    let mut recovered = graph
        .recover_expired(unix_time_ms())
        .map_err(graph_failed)?;
    // A turn found open is proof its process is gone, so whatever task it held
    // is handed back now rather than when the lease would have run out.
    if !interrupted.is_empty() {
        recovered.extend(graph.reclaim_running().map_err(graph_failed)?);
    }
    let waiting = graph.pending().first().map(|node| node.id);
    drop(graph);

    let mut report = json!({
        "session": session.to_string(),
        "events": version.0,
        "closed_turns": interrupted.len(),
        "recovered_tasks": recovered.len(),
        "continuing": Value::Null,
        // ponytail: following live events needs the serve loop; the flag is
        // accepted and reports the committed head instead of hanging.
        "following": follow,
    });

    let Some(task) = waiting else {
        emitter.result(report);
        return Ok(0);
    };
    // The session has unfinished work. Continuing it needs a provider, so a
    // workspace that cannot dispatch reports the task as still waiting rather
    // than losing it.
    let mut execution = match TaskRun::open(invocation, Some(session)) {
        Ok(execution) => execution,
        Err(diagnostic) => {
            let code = unusable(diagnostic, emitter);
            merge(&mut report, json!({"blocked": task.to_string()}));
            emitter.result(report);
            return Ok(code);
        }
    };
    merge(&mut report, json!({"continuing": task.to_string()}));
    execution.execute(task, report, emitter)
}

pub(crate) fn doctor(invocation: &Invocation, strict: bool, emitter: &mut Emitter) -> i32 {
    let mut warnings: Vec<Diagnostic> = Vec::new();
    let workspace = workspace_root(&invocation.workspace);
    let storage = match &workspace {
        Ok(root) => match open_store(root) {
            Ok(_) => "openable".to_owned(),
            Err(error) => {
                warnings.push(error.clone());
                error.message
            }
        },
        Err(error) => {
            warnings.push(error.clone());
            error.message.clone()
        }
    };

    // ponytail: layer discovery only. Merged values and their source trace
    // arrive with `arsy config explain`.
    //
    // Bootstrapped first, so the user layer is reported as it will be for
    // every later command rather than as absent on the run that creates it.
    bootstrap_user_config();
    let root = workspace.as_deref().unwrap_or(Path::new("."));
    let config: Vec<Value> = arsy_kernel::config::layers(root, root)
        .into_iter()
        .map(|(layer, path)| {
            json!({
                "layer": layer,
                "path": path.display().to_string(),
                "present": path.is_file(),
            })
        })
        .collect();

    // A configured endpoint with a reachable credential is what decides
    // whether a turn can dispatch, so report it as one fact rather than
    // leaving an operator to infer it from the credential count.
    let mut model_insight = Value::Null;
    let provider = match load_config(root, root, invocation.config.as_deref()) {
        Err(diagnostic) => {
            let value = json!({"status": "unusable", "detail": diagnostic.message});
            warnings.push(diagnostic);
            value
        }
        Ok(config) => {
            // Doctor never probes: it reports cached health and free specs.
            let insight = crate::probelm::gather(&config, false);
            if let Some(note) = &insight.note {
                warnings.push(crate::probelm::unavailable(note));
            }
            model_insight = insight.report();
            match provider::resolve(&config, None, &insight) {
                Ok(resolved) => json!({
                    "status": "ready",
                    "id": resolved.endpoint.id,
                    "kind": resolved.endpoint.kind.as_str(),
                    "base_url": resolved.endpoint.base_url,
                    "credential_source": resolved.source.as_str(),
                    // Present only when `provider.default = "auto"` left the choice
                    // to routing; naming the criterion is what makes the choice
                    // reviewable rather than surprising.
                    "routing": resolved.route.as_ref().map(|decision| match decision {
                        arsy_kernel::routing::Decision::Routed { key, reasons, excluded } => json!({
                            "model": key.to_string(),
                            "reasons": reasons,
                            "excluded": excluded.len(),
                        }),
                        other => serde_json::to_value(other).unwrap_or(Value::Null),
                    }),
                }),
                Err(diagnostic) => json!({"status": "unavailable", "detail": diagnostic.message}),
            }
        }
    };

    let sandbox_assurance = installed_sandbox_assurance();
    if sandbox_assurance == arsy_kernel::policy::SandboxAssurance::None {
        warnings.push(Diagnostic::warning(
            "ARSY-SBX-1000",
            "no complete sandbox worker is available, so achieved assurance is `none`",
            "install arsy-sandbox-worker and the platform controls before running effects",
        ));
    }
    let credentials = catalog().unwrap_or_default().len();
    if credentials == 0 {
        warnings.push(Diagnostic::warning(
            "ARSY-PRV-1001",
            "no provider credential is stored",
            "store a credential with `arsy auth set <PROVIDER>`",
        ));
    }

    for warning in &warnings {
        emitter.diagnostic(warning);
    }
    emitter.result(json!({
        "platform": platform(),
        "version": arsy_code::VERSION,
        "sandbox_assurance": sandbox_assurance.as_str(),
        "provider_auth": if credentials == 0 { "none" } else { "configured" },
        "provider": provider,
        "model_insight": model_insight,
        "storage": storage,
        "config_layers": config,
        "warnings": warnings.len(),
    }));

    // Warnings return 0 unless --strict; then the first reported one is
    // terminal and selects the code.
    match (strict, warnings.first()) {
        (true, Some(terminal)) => terminal.exit_code(),
        _ => 0,
    }
}

/// Consume one round's event stream, returning the complete tool calls it
/// carried.
///
/// Extracted from `dispatch`'s round loop so the round loop stays a loop over
/// rounds and the event kinds are handled in one place.
#[allow(clippy::too_many_arguments)]
fn absorb_stream_events(
    stream: impl Iterator<Item = Result<ModelEvent, ProviderError>>,
    emitter: &mut Emitter,
    answer: &mut String,
    input_tokens: &mut u64,
    output_tokens: &mut u64,
    round_input: &mut u64,
    round_output: &mut u64,
    round: usize,
) -> Result<Vec<(String, String, Value)>, ProviderError> {
    let mut calls = Vec::new();
    for event in stream {
        match event? {
            ModelEvent::TextDelta { text } => {
                emitter.delta(&text);
                answer.push_str(&text);
            }
            ModelEvent::Usage {
                input_tokens: input,
                output_tokens: output,
            } => {
                *round_input += input;
                *round_output += output;
                *input_tokens += input;
                *output_tokens += output;
            }
            ModelEvent::ToolCallCompleted {
                id,
                name,
                arguments,
                ..
            } => {
                emitter.trace(
                    "model.tool_call",
                    json!({"round": round, "id": id, "name": name, "arguments": arguments}),
                );
                calls.push((id, name, arguments));
            }
            ModelEvent::Completed { .. }
            | ModelEvent::ToolCallStarted { .. }
            | ModelEvent::ToolCallDelta { .. }
            // `arsy run` is a scriptable surface: reasoning is for the
            // operator watching a stream, not for a pipeline's stdout.
            | ModelEvent::ThinkingDelta { .. } => {}
        }
    }
    Ok(calls)
}
