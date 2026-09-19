//! Embedded agent service: the single session coordinator.
//!
//! It is the only component permitted to append canonical events for a
//! session, so turn lifecycle, idempotency, and subscription delivery are all
//! decided in one place; see `docs/04-system-architecture.md`.

use crate::{
    capability::CapabilityGrant,
    domain::{CorrelationId, EventId, Principal, SessionId, StateVersion, SubscriptionId, TurnId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
    projection::{ProjectionError, ProjectionSet, TurnStatus, UsageTotals},
    protocol::{
        ClientRequest, IdempotencyKey, ProtocolEnvelope, ProtocolError, RequestLedger, ServerEvent,
        SubscriptionCursor, MAX_SUBSCRIPTION_BATCH,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

/// Schema version stamped on every event this service appends.
pub const SERVICE_SCHEMA: SchemaVersion = SchemaVersion(1);
/// Events buffered per subscriber before it is fast-forwarded with a gap.
pub const MAX_SUBSCRIBER_QUEUE: usize = MAX_SUBSCRIPTION_BATCH;

/// Evidence recorded with `turn.started`: what the client asked for, and under
/// which key, so a replay can be proved rather than assumed.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnStartedEvidence {
    pub turn_id: TurnId,
    pub prompt: String,
    pub request_digest: StateVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

/// Evidence recorded when a turn leaves the running state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnFinishedEvidence {
    pub turn_id: TurnId,
    pub started_sequence: u64,
    pub outcome_digest: StateVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnFailure>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnFailure {
    pub code: String,
    pub message: String,
}

/// How a new stream relates to the one it came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchMode {
    /// The branch continues from the parent's state at the branch point: a
    /// reader reconstructs its history by walking ancestry into the parent's
    /// prefix. Nothing is copied and nothing is truncated.
    Rewind,
    /// The branch records where it came from but inherits no history.
    Fork,
}

impl BranchMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rewind => "rewind",
            Self::Fork => "fork",
        }
    }

    /// Whether the parent's prefix is part of this branch's history.
    pub const fn inherits_prefix(self) -> bool {
        matches!(self, Self::Rewind)
    }
}

/// Evidence recorded with `session.branched`: enough to replay the branch's
/// history without consulting anything but the store.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchEvidence {
    pub mode: BranchMode,
    pub parent: SessionId,
    /// The parent event the branch continues from.
    pub at_event: EventId,
    pub at_sequence: u64,
    /// The parent's committed length when the branch was taken, so a later
    /// reader can tell how much of the parent the branch does not include.
    pub parent_version: u64,
}

/// Verdict for a `turn_start` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnAdmission {
    pub turn: TurnId,
    /// True when the idempotency key was already admitted: nothing was appended
    /// and the caller must not execute the turn again.
    pub replay: bool,
}

pub struct AgentService {
    store: Arc<dyn EventStore>,
    session: SessionId,
    state: Mutex<ServiceState>,
}

struct ServiceState {
    version: StreamVersion,
    projection: ProjectionSet,
    ledger: RequestLedger,
    turns_by_key: HashMap<IdempotencyKey, TurnId>,
    started_at: HashMap<TurnId, u64>,
    subscribers: BTreeMap<SubscriptionId, Subscriber>,
}

impl AgentService {
    /// Attach to a session, rebuilding lifecycle and idempotency state from the
    /// committed events. A crash mid-turn therefore resumes from the last
    /// committed event: unfinished turns stay visible via `unfinished_turns`.
    pub fn attach(store: Arc<dyn EventStore>, session: SessionId) -> Result<Self, ServiceError> {
        let mut projection = ProjectionSet::new(session);
        let mut ledger = RequestLedger::new();
        let mut turns_by_key = HashMap::new();
        let mut started_at = HashMap::new();
        let mut next = 1;

        loop {
            let page = store.read(session, next, MAX_SUBSCRIPTION_BATCH)?;
            if page.is_empty() {
                break;
            }
            for event in &page {
                projection.apply(event)?;
                if event.kind == TURN_STARTED {
                    let evidence: TurnStartedEvidence = inline(event)?;
                    started_at.insert(evidence.turn_id, event.sequence);
                    if let Some(key) = evidence.idempotency_key {
                        ledger.record(key.clone(), evidence.request_digest);
                        turns_by_key.insert(key, evidence.turn_id);
                    }
                }
                next = event
                    .sequence
                    .checked_add(1)
                    .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?;
            }
        }

        Ok(Self {
            store,
            session,
            state: Mutex::new(ServiceState {
                version: projection.applied_version(),
                projection,
                ledger,
                turns_by_key,
                started_at,
                subscribers: BTreeMap::new(),
            }),
        })
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Read one stream in full, in sequence order.
    pub fn history(
        store: &dyn EventStore,
        session: SessionId,
    ) -> Result<Vec<EventEnvelope>, ServiceError> {
        let mut events = Vec::new();
        let mut next = 1;
        loop {
            let page = store.read(session, next, MAX_SUBSCRIPTION_BATCH)?;
            let Some(last) = page.last() else {
                return Ok(events);
            };
            next = last
                .sequence
                .checked_add(1)
                .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?;
            events.extend(page);
        }
    }

    /// Open a new stream that records where it came from.
    ///
    /// History is never rewritten: the parent keeps every event it had, and the
    /// branch begins with one `session.branched` event naming the parent and
    /// the branch point. A `Rewind` branch continues from the parent's state at
    /// that point; a `Fork` records the ancestry only.
    ///
    /// `at` defaults to the parent's last committed event.
    pub fn branch(
        store: Arc<dyn EventStore>,
        parent: SessionId,
        at: Option<EventId>,
        mode: BranchMode,
        actor: Principal,
    ) -> Result<(Self, SessionId, BranchEvidence), ServiceError> {
        let history = Self::history(store.as_ref(), parent)?;
        let branch_point = match at {
            Some(id) => history
                .iter()
                .find(|event| event.id == id)
                .ok_or(ServiceError::UnknownEvent(id))?,
            None => history.last().ok_or(ServiceError::EmptyStream(parent))?,
        };
        let evidence = BranchEvidence {
            mode,
            parent,
            at_event: branch_point.id,
            at_sequence: branch_point.sequence,
            parent_version: history.last().map_or(0, |event| event.sequence),
        };

        let session = SessionId::new();
        let service = Self::attach(store, session)?;
        {
            let mut state = service.lock()?;
            service.append(&mut state, actor, SESSION_BRANCHED, &evidence)?;
        }
        Ok((service, session, evidence))
    }

    /// The `session.branched` evidence this stream opened with, when it is a
    /// branch of another one.
    pub fn ancestry(
        store: &dyn EventStore,
        session: SessionId,
    ) -> Result<Option<BranchEvidence>, ServiceError> {
        let first = store.read(session, 1, 1)?;
        match first.first() {
            Some(event) if event.kind == SESSION_BRANCHED => Ok(Some(inline(event)?)),
            _ => Ok(None),
        }
    }

    /// Admit a `turn_start` request and append `turn.started`.
    ///
    /// A repeated idempotency key with the same body returns the original turn
    /// without appending; the same key with a different body is a conflict.
    pub fn start_turn(
        &self,
        actor: Principal,
        envelope: &ProtocolEnvelope<ClientRequest>,
    ) -> Result<TurnAdmission, ServiceError> {
        let ClientRequest::TurnStart(request) = &envelope.payload else {
            return Err(ServiceError::UnexpectedMethod);
        };
        if request.session != self.session {
            return Err(ServiceError::WrongSession);
        }
        let digest = envelope.request_digest()?;
        let mut state = self.lock()?;

        if let Some(key) = &envelope.idempotency_key {
            // `admit` owns conflict detection; the turn map only recalls the outcome.
            if state.ledger.admit(envelope)?.is_replay() {
                let turn = *state
                    .turns_by_key
                    .get(key)
                    .ok_or(ServiceError::LedgerDesync)?;
                return Ok(TurnAdmission { turn, replay: true });
            }
        }

        let turn = TurnId::new();
        let evidence = TurnStartedEvidence {
            turn_id: turn,
            // ponytail: prompts above the inline event bound are rejected by the
            // store; move the body to the artifact CAS when P2 owns prompt bodies.
            prompt: request.prompt.clone(),
            request_digest: digest,
            idempotency_key: envelope.idempotency_key.clone(),
        };
        let sequence = self.append(&mut state, actor, TURN_STARTED, &evidence)?;
        state.started_at.insert(turn, sequence);
        if let Some(key) = envelope.idempotency_key.clone() {
            state.turns_by_key.insert(key, turn);
        }
        Ok(TurnAdmission {
            turn,
            replay: false,
        })
    }

    /// Append `turn.completed` with the digest of the turn outcome.
    pub fn complete_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        outcome: &Value,
    ) -> Result<StreamVersion, ServiceError> {
        self.finish_turn(actor, turn, TURN_COMPLETED, outcome, None)
    }

    /// Append `turn.failed`; a resumed service uses this to close a turn that
    /// was still running when the process died.
    pub fn fail_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<StreamVersion, ServiceError> {
        let failure = TurnFailure {
            code: code.into(),
            message: message.into(),
        };
        self.finish_turn(actor, turn, TURN_FAILED, &Value::Null, Some(failure))
    }

    /// Append `usage.recorded`, so what a turn spent outlives the process that
    /// spent it.
    ///
    /// Separate from `complete_turn` because a turn that fails has still spent
    /// tokens, and because the projection folds usage across turns: totals
    /// belong to the session, not to whichever turn happened to finish last.
    pub fn record_usage(
        &self,
        actor: Principal,
        usage: UsageTotals,
    ) -> Result<StreamVersion, ServiceError> {
        let mut state = self.lock()?;
        self.append(
            &mut state,
            actor,
            USAGE_RECORDED,
            &json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cost_micros": usage.cost_micros,
            }),
        )?;
        Ok(state.version)
    }

    /// Record what a turn said, as history rather than as evidence.
    ///
    /// `turn.completed` carries only the *digest* of a turn's outcome, which is
    /// what makes it tamper-evident and useless for resuming: a digest cannot
    /// be read back into a conversation. So the exchange is its own event, with
    /// its own inline payload, appended before the turn is closed.
    ///
    /// Written for a turn that is about to complete. A turn that failed or was
    /// interrupted records nothing, so replaying the stream cannot restore half
    /// an exchange the model never finished having.
    pub fn record_transcript(
        &self,
        actor: Principal,
        turn: TurnId,
        transcript: &Value,
    ) -> Result<StreamVersion, ServiceError> {
        let mut state = self.lock()?;
        self.append(
            &mut state,
            actor,
            TURN_TRANSCRIPT,
            &json!({"turn_id": turn, "transcript": transcript}),
        )?;
        Ok(state.version)
    }

    /// Record a bounded rule the operator approved from the interactive card.
    ///
    /// The grants are the exact leaf capabilities the card displayed. Keeping
    /// them as their own event makes a later audit distinguish one-call
    /// approval from session reuse without widening the turn outcome contract.
    pub fn record_approval(
        &self,
        actor: Principal,
        turn: TurnId,
        grants: &[CapabilityGrant],
    ) -> Result<StreamVersion, ServiceError> {
        let mut state = self.lock()?;
        self.append(
            &mut state,
            actor,
            APPROVAL_GRANTED,
            &json!({"turn_id": turn, "grants": grants}),
        )?;
        Ok(state.version)
    }

    fn finish_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        kind: &str,
        outcome: &Value,
        failure: Option<TurnFailure>,
    ) -> Result<StreamVersion, ServiceError> {
        let mut state = self.lock()?;
        let running = state
            .projection
            .turns()
            .get(&turn)
            .ok_or(ServiceError::UnknownTurn(turn))?
            .status
            == TurnStatus::Running;
        if !running {
            return Err(ServiceError::TurnNotRunning(turn));
        }
        let started_sequence = *state
            .started_at
            .get(&turn)
            .ok_or(ServiceError::UnknownTurn(turn))?;
        let evidence = TurnFinishedEvidence {
            turn_id: turn,
            started_sequence,
            outcome_digest: digest_of(outcome)?,
            failure,
        };
        self.append(&mut state, actor, kind, &evidence)?;
        Ok(state.version)
    }

    /// Turns that were running at the last committed event.
    pub fn unfinished_turns(&self) -> Result<Vec<TurnId>, ServiceError> {
        let state = self.lock()?;
        Ok(state
            .projection
            .turns()
            .values()
            .filter(|turn| turn.status == TurnStatus::Running)
            .map(|turn| turn.id)
            .collect())
    }

    pub fn committed_version(&self) -> Result<StreamVersion, ServiceError> {
        Ok(self.lock()?.version)
    }

    /// Register a subscriber and hand back its bounded catch-up snapshot.
    ///
    /// At most `MAX_SUBSCRIPTION_BATCH` committed events are replayed; anything
    /// older than that is reported as a single gap the client must reconcile.
    pub fn subscribe(&self, from_sequence: u64) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut state = self.lock()?;
        let snapshot = self
            .store
            .read(self.session, from_sequence, MAX_SUBSCRIPTION_BATCH)?;
        let subscription = SubscriptionId::new();
        let mut cursor = SubscriptionCursor::resume(subscription, from_sequence);
        let mut events = vec![ServerEvent::Subscribed {
            subscription,
            next_sequence: from_sequence,
        }];
        for envelope in snapshot {
            cursor.accept(&envelope)?;
            events.push(ServerEvent::Stream {
                subscription,
                envelope: Box::new(envelope),
            });
        }
        if let Some(head) = state.version.0.checked_add(1) {
            if let Some(gap) = cursor.fast_forward(head) {
                events.push(gap);
            }
        }
        state.subscribers.insert(
            subscription,
            Subscriber {
                cursor,
                pending: VecDeque::new(),
                gap: None,
            },
        );
        Ok(events)
    }

    /// Drain everything buffered for a subscriber since its last poll.
    pub fn poll(&self, subscription: SubscriptionId) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut state = self.lock()?;
        state
            .subscribers
            .get_mut(&subscription)
            .ok_or(ServiceError::UnknownSubscription(subscription))?
            .drain()
    }

    pub fn unsubscribe(&self, subscription: SubscriptionId) -> Result<(), ServiceError> {
        self.lock()?.subscribers.remove(&subscription);
        Ok(())
    }

    /// Append one event, then fan it out. Fan-out only touches per-subscriber
    /// buffers, so a slow subscriber can never block or fail a commit.
    ///
    /// The service is not the only writer a session has — a task graph records
    /// its own state transitions to the same stream, because resuming a task
    /// means replaying one history rather than correlating two. A conflict is
    /// therefore the ordinary case of "someone else appended since we looked":
    /// the missed events are folded into the projection and the append retried
    /// once at the sequence that is now free. A second conflict is a genuinely
    /// contended stream and is reported as one.
    fn append<T: Serialize>(
        &self,
        state: &mut ServiceState,
        actor: Principal,
        kind: &str,
        payload: &T,
    ) -> Result<u64, ServiceError> {
        let data = serde_json::to_value(payload)
            .map_err(|error| ServiceError::Store(StoreError::Serialization(error.to_string())))?;
        for attempt in 0..2 {
            let sequence = state
                .version
                .0
                .checked_add(1)
                .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?;
            let envelope = EventEnvelope::new(
                self.session,
                sequence,
                actor.clone(),
                state.last_event_id(),
                CorrelationId::new(),
                SERVICE_SCHEMA,
                kind,
                EventPayload::Inline { data: data.clone() },
            );
            match self
                .store
                .append(self.session, state.version, vec![envelope.clone()])
            {
                Ok(version) => {
                    state.projection.apply(&envelope)?;
                    state.version = version;
                    for subscriber in state.subscribers.values_mut() {
                        subscriber.offer(&envelope);
                    }
                    return Ok(sequence);
                }
                Err(StoreError::Conflict { .. }) if attempt == 0 => self.catch_up(state)?,
                Err(error) => return Err(error.into()),
            }
        }
        Err(ServiceError::Store(StoreError::Conflict {
            expected: state.version,
            actual: self.store.current_version(self.session)?,
        }))
    }

    /// Fold everything appended to this session since the service last looked
    /// into its projection and its subscribers.
    fn catch_up(&self, state: &mut ServiceState) -> Result<(), ServiceError> {
        loop {
            let page = self.store.read(
                self.session,
                state
                    .version
                    .0
                    .checked_add(1)
                    .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?,
                MAX_SUBSCRIPTION_BATCH,
            )?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            let version = StreamVersion(last.sequence);
            for event in &page {
                state.projection.apply(event)?;
                for subscriber in state.subscribers.values_mut() {
                    subscriber.offer(event);
                }
            }
            state.version = version;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ServiceState>, ServiceError> {
        self.state.lock().map_err(|_| ServiceError::Poisoned)
    }
}

impl ServiceState {
    fn last_event_id(&self) -> Option<EventId> {
        self.projection.audit().last().map(|entry| entry.event_id)
    }
}

/// One subscriber's bounded buffer. Overflow collapses into a single gap so
/// memory stays bounded regardless of how far behind the client falls.
struct Subscriber {
    cursor: SubscriptionCursor,
    pending: VecDeque<EventEnvelope>,
    gap: Option<ServerEvent>,
}

impl Subscriber {
    fn offer(&mut self, envelope: &EventEnvelope) {
        let expected = self
            .cursor
            .next_sequence()
            .saturating_add(self.pending.len() as u64);
        if envelope.sequence != expected {
            return;
        }
        if self.pending.len() >= MAX_SUBSCRIBER_QUEUE {
            self.pending.clear();
            if let Some(next) = envelope.sequence.checked_add(1) {
                // Repeated overflows coalesce into one gap that still starts at
                // the oldest sequence the client never saw.
                let oldest = match self.gap.take() {
                    Some(ServerEvent::Gap { dropped_from, .. }) => Some(dropped_from),
                    _ => None,
                };
                self.gap = self
                    .cursor
                    .fast_forward(next)
                    .map(|gap| match (oldest, gap) {
                        (
                            Some(dropped_from),
                            ServerEvent::Gap {
                                subscription,
                                next_sequence,
                                ..
                            },
                        ) => ServerEvent::Gap {
                            subscription,
                            dropped_from,
                            next_sequence,
                        },
                        (_, gap) => gap,
                    });
            }
            return;
        }
        self.pending.push_back(envelope.clone());
    }

    fn drain(&mut self) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut events = Vec::with_capacity(self.pending.len() + 1);
        if let Some(gap) = self.gap.take() {
            events.push(gap);
        }
        let subscription = self.cursor.subscription();
        for envelope in std::mem::take(&mut self.pending) {
            self.cursor.accept(&envelope)?;
            events.push(ServerEvent::Stream {
                subscription,
                envelope: Box::new(envelope),
            });
        }
        Ok(events)
    }
}

const SESSION_BRANCHED: &str = "session.branched";
const TURN_STARTED: &str = "turn.started";
const TURN_COMPLETED: &str = "turn.completed";
const TURN_FAILED: &str = "turn.failed";
const USAGE_RECORDED: &str = "usage.recorded";
const APPROVAL_GRANTED: &str = "approval.granted";
/// What a turn said, as replayable history. Not evidence: `turn.completed`
/// carries the digest that makes the outcome tamper-evident, and a digest
/// cannot be read back into a conversation.
pub const TURN_TRANSCRIPT: &str = "turn.transcript";

fn digest_of(value: &Value) -> Result<StateVersion, ServiceError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ServiceError::Store(StoreError::Serialization(error.to_string())))?;
    Ok(StateVersion::from_digest(Sha256::digest(&bytes).into()))
}

fn inline<T: serde::de::DeserializeOwned>(event: &EventEnvelope) -> Result<T, ServiceError> {
    let EventPayload::Inline { data } = &event.payload else {
        return Err(ServiceError::MissingEvidence(event.kind.clone()));
    };
    serde_json::from_value(data.clone())
        .map_err(|_| ServiceError::MissingEvidence(event.kind.clone()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    Store(StoreError),
    Projection(ProjectionError),
    Protocol(ProtocolError),
    UnexpectedMethod,
    WrongSession,
    UnknownTurn(TurnId),
    TurnNotRunning(TurnId),
    UnknownSubscription(SubscriptionId),
    UnknownEvent(EventId),
    EmptyStream(SessionId),
    MissingEvidence(String),
    LedgerDesync,
    Poisoned,
}

impl From<StoreError> for ServiceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<ProjectionError> for ServiceError {
    fn from(value: ProjectionError) -> Self {
        Self::Projection(value)
    }
}

impl From<ProtocolError> for ServiceError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "event store: {error}"),
            Self::Projection(error) => write!(formatter, "projection: {error}"),
            Self::Protocol(error) => write!(formatter, "protocol: {error}"),
            Self::UnexpectedMethod => formatter.write_str("request is not a turn_start"),
            Self::WrongSession => formatter.write_str("request targets another session"),
            Self::UnknownTurn(turn) => write!(formatter, "turn {turn} does not exist"),
            Self::TurnNotRunning(turn) => write!(formatter, "turn {turn} is already finished"),
            Self::UnknownSubscription(id) => write!(formatter, "subscription {id} does not exist"),
            Self::UnknownEvent(id) => write!(formatter, "event {id} is not in this session"),
            Self::EmptyStream(session) => {
                write!(formatter, "session {session} has no recorded events")
            }
            Self::MissingEvidence(kind) => {
                write!(formatter, "{kind} is missing its inline evidence")
            }
            Self::LedgerDesync => {
                formatter.write_str("idempotency key was admitted without a recorded turn")
            }
            Self::Poisoned => formatter.write_str("agent service lock poisoned"),
        }
    }
}

impl std::error::Error for ServiceError {}
