//! `validate.*`: what was checked, what it was checked against, and whether
//! that check still holds.
//!
//! A validation is the evidence a completion claim cites, so it has to survive
//! the process that produced it: a log held in memory answers "did this pass"
//! only until a restart, and a restarted agent that cannot see the last check
//! either re-runs it or, worse, asserts it. So records are written to the
//! session's event stream, like [`todo`](crate::todo) and the task graph, and
//! rebuilt by replaying it.
//!
//! Each record names the artifact the outcome was read from, a digest of the
//! command, the task and attempt it belongs to, the grants that authorized it,
//! and the workspace revision it ran against. The last of those is what makes
//! staleness answerable: a pass recorded against a revision the workspace has
//! since moved off is still true about that revision, and no longer evidence
//! about this one.

use crate::{
    domain::{
        ArtifactId, AttemptId, CorrelationId, GrantId, Principal, SessionId, StateVersion, TaskId,
        WorkspaceVersion,
    },
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fmt, sync::Arc};

/// Events replayed per catch-up read, matching the rest of the kernel.
const MAX_CATCH_UP_BATCH: usize = 256;

/// Wire version of [`ValidationRecord`].
pub const VALIDATION_SCHEMA_VERSION: u32 = 1;

/// Longest excerpt of a check's own output kept against a record. Enough to
/// show the failing assertion, little enough that a noisy test runner cannot
/// crowd out the session stream it is written to.
pub const MAX_DETAIL_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationOutcome {
    Passed,
    Failed,
}

/// One recorded check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationRecord {
    pub schema: u32,
    pub sequence: u64,
    /// The operation that produced the evidence, so a record is traceable to
    /// the authorized call rather than to a claim about one.
    pub operation: String,
    pub command: String,
    /// Digest of the command text, computed here rather than supplied: it is
    /// what lets a later reader tell "the same check" from "a check with the
    /// same name".
    pub command_digest: StateVersion,
    /// The process-result artifact the outcome was read from.
    pub artifact: ArtifactId,
    pub task: Option<TaskId>,
    pub attempt: Option<AttemptId>,
    /// The grants the recording call held.
    pub grants: Vec<GrantId>,
    /// The workspace revision the check ran against. `None` when the workspace
    /// cannot report one — not a match with anything.
    pub workspace_revision: Option<WorkspaceVersion>,
    pub outcome: ValidationOutcome,
    pub detail: String,
    pub recorded_at_ms: u64,
}

/// What a caller has to say to record a check.
#[derive(Clone, Debug)]
pub struct NewValidation {
    pub operation: String,
    pub command: String,
    pub artifact: ArtifactId,
    pub task: Option<TaskId>,
    pub attempt: Option<AttemptId>,
    pub grants: Vec<GrantId>,
    pub workspace_revision: Option<WorkspaceVersion>,
    pub outcome: ValidationOutcome,
    pub detail: String,
    pub recorded_at_ms: u64,
}

/// Where the last check leaves the work it was checking.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    /// Nothing has been checked yet.
    Unvalidated,
    Passed,
    Failed,
    /// The last check passed, against a workspace revision this is no longer.
    /// The record stays on the log; it just cannot answer for this revision.
    Stale,
}

impl ValidationState {
    /// Whether there is still something for an agent to do before it may claim
    /// the work is checked.
    pub const fn actionable(self) -> bool {
        !matches!(self, Self::Passed)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationStatus {
    pub state: ValidationState,
    pub actionable: bool,
    /// Whether the records survive a restart. `false` is a log with nowhere
    /// durable to write — a dry run or a bare tool call — said out loud rather
    /// than silently presented as evidence.
    pub durable: bool,
    pub records: Vec<ValidationRecord>,
}

/// A session's validation log, rebuilt from its event stream.
pub struct ValidationLog {
    store: Option<Arc<dyn EventStore>>,
    session: SessionId,
    actor: Principal,
    version: StreamVersion,
    records: Vec<ValidationRecord>,
}

impl ValidationLog {
    /// Read the session's stream and rebuild the log it left.
    pub fn open(
        store: Arc<dyn EventStore>,
        session: SessionId,
        actor: Principal,
    ) -> Result<Self, ValidationError> {
        let mut log = Self {
            store: Some(store),
            session,
            actor,
            version: StreamVersion(0),
            records: Vec::new(),
        };
        log.catch_up()?;
        Ok(log)
    }

    /// A log with nowhere to write, for a caller that has no session stream —
    /// a dry run, a bare tool call, a test. Its status says `durable: false`,
    /// because evidence that disappears on restart must not read like evidence
    /// that does not.
    pub fn ephemeral() -> Self {
        Self {
            store: None,
            session: SessionId::new(),
            actor: Principal::System,
            version: StreamVersion(0),
            records: Vec::new(),
        }
    }

    pub fn records(&self) -> &[ValidationRecord] {
        &self.records
    }

    /// The log as it answers for one workspace revision.
    pub fn status(&self, workspace_revision: Option<WorkspaceVersion>) -> ValidationStatus {
        let state = match self.records.last() {
            None => ValidationState::Unvalidated,
            Some(record) => match record.outcome {
                ValidationOutcome::Failed => ValidationState::Failed,
                ValidationOutcome::Passed => {
                    // Only a revision mismatch makes a pass stale. An unknown
                    // revision on either side cannot establish a mismatch, and
                    // is not treated as one.
                    match (record.workspace_revision, workspace_revision) {
                        (Some(recorded), Some(current)) if recorded != current => {
                            ValidationState::Stale
                        }
                        _ => ValidationState::Passed,
                    }
                }
            },
        };
        ValidationStatus {
            state,
            actionable: state.actionable(),
            durable: self.store.is_some(),
            records: self.records.clone(),
        }
    }

    /// Write one check to the log.
    pub fn record(&mut self, new: NewValidation) -> Result<ValidationRecord, ValidationError> {
        if new.command.trim().is_empty() {
            return Err(ValidationError::MissingCommand);
        }
        let mut detail = new.detail;
        detail.truncate(MAX_DETAIL_BYTES);
        let record = ValidationRecord {
            schema: VALIDATION_SCHEMA_VERSION,
            sequence: self.records.len() as u64 + 1,
            command_digest: StateVersion::from_digest(
                Sha256::digest(new.command.as_bytes()).into(),
            ),
            operation: new.operation,
            command: new.command,
            artifact: new.artifact,
            task: new.task,
            attempt: new.attempt,
            grants: new.grants,
            workspace_revision: new.workspace_revision,
            outcome: new.outcome,
            detail,
            recorded_at_ms: new.recorded_at_ms,
        };
        let Some(store) = self.store.clone() else {
            self.records.push(record.clone());
            return Ok(record);
        };
        for attempt in 0..2 {
            // Rebuilt inside the loop: a catch-up may have replayed another
            // writer's record, and this one's sequence follows it.
            let record = ValidationRecord {
                sequence: self.records.len() as u64 + 1,
                ..record.clone()
            };
            let sequence = self
                .version
                .0
                .checked_add(1)
                .ok_or(ValidationError::Overflow)?;
            let event = EventEnvelope::new(
                self.session,
                sequence,
                self.actor.clone(),
                None,
                CorrelationId::new(),
                SchemaVersion(1),
                "validation.recorded",
                EventPayload::Inline {
                    data: json!({"record": &record}),
                },
            );
            match store.append(self.session, self.version, vec![event]) {
                Ok(version) => {
                    self.version = version;
                    self.records.push(record.clone());
                    return Ok(record);
                }
                Err(StoreError::Conflict { .. }) if attempt == 0 => self.catch_up()?,
                Err(error) => return Err(ValidationError::Store(error)),
            }
        }
        Err(ValidationError::Contended)
    }

    fn catch_up(&mut self) -> Result<(), ValidationError> {
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        loop {
            let from = self
                .version
                .0
                .checked_add(1)
                .ok_or(ValidationError::Overflow)?;
            let page = store
                .read(self.session, from, MAX_CATCH_UP_BATCH)
                .map_err(ValidationError::Store)?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            let version = StreamVersion(last.sequence);
            self.replay_page(&page);
            self.version = version;
        }
    }

    /// Fold one page of at most [`MAX_CATCH_UP_BATCH`] events into the log.
    /// Each event is read once; a page is read once; the whole catch-up is one
    /// pass over the stream.
    fn replay_page(&mut self, page: &[EventEnvelope]) {
        for event in page {
            self.replay(event);
        }
    }

    /// Fold one event into the log, skipping what cannot be read.
    ///
    /// A record that fails to deserialize is one piece of evidence lost, and
    /// refusing to open the session would lose the rest of it as well. What is
    /// never done is inventing a pass for it.
    fn replay(&mut self, event: &EventEnvelope) {
        let EventPayload::Inline { data } = &event.payload else {
            return;
        };
        if event.kind != "validation.recorded" {
            return;
        }
        if let Some(record) = data
            .get("record")
            .cloned()
            .and_then(|value| serde_json::from_value::<ValidationRecord>(value).ok())
        {
            self.records.push(record);
        }
    }
}

#[derive(Debug)]
pub enum ValidationError {
    MissingCommand,
    Contended,
    Overflow,
    Store(StoreError),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => {
                formatter.write_str("a validation record needs the command that was run")
            }
            Self::Contended => formatter.write_str("the validation log's stream is contended"),
            Self::Overflow => formatter.write_str("validation event sequence overflow"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ValidationError {}

/// The digest a caller can compare a record's `command_digest` against.
pub fn command_digest(command: &str) -> StateVersion {
    StateVersion::from_digest(Sha256::digest(command.as_bytes()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MemoryEventStore;

    fn revision(byte: u8) -> WorkspaceVersion {
        WorkspaceVersion(StateVersion::from_digest([byte; 32]))
    }

    fn passing(command: &str, revision: Option<WorkspaceVersion>) -> NewValidation {
        NewValidation {
            operation: "validate.record".into(),
            command: command.into(),
            artifact: ArtifactId::new(),
            task: Some(TaskId::new()),
            attempt: Some(AttemptId::new()),
            grants: vec![GrantId::new()],
            workspace_revision: revision,
            outcome: ValidationOutcome::Passed,
            detail: String::new(),
            recorded_at_ms: 7,
        }
    }

    #[test]
    fn a_pass_survives_a_restart_and_goes_stale_when_the_workspace_moves() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut log = ValidationLog::open(Arc::clone(&store), session, Principal::System).unwrap();
        log.record(NewValidation {
            outcome: ValidationOutcome::Failed,
            ..passing("cargo test", Some(revision(1)))
        })
        .unwrap();
        let recorded = log
            .record(passing("cargo test", Some(revision(1))))
            .unwrap();
        assert_eq!(recorded.sequence, 2);
        assert_eq!(recorded.command_digest, command_digest("cargo test"));

        let rebuilt = ValidationLog::open(store, session, Principal::System).unwrap();
        assert_eq!(rebuilt.records().len(), 2, "the log outlives the process");
        let status = rebuilt.status(Some(revision(1)));
        assert_eq!(status.state, ValidationState::Passed);
        assert!(!status.actionable);
        assert!(status.durable);
        assert_eq!(status.records[0].outcome, ValidationOutcome::Failed);

        let moved = rebuilt.status(Some(revision(2)));
        assert_eq!(moved.state, ValidationState::Stale);
        assert!(
            moved.actionable,
            "a pass about another revision is not one about this"
        );
        assert_eq!(moved.records.len(), 2, "stale evidence stays visible");
    }

    #[test]
    fn an_ephemeral_log_says_it_is_not_durable_and_refuses_an_empty_command() {
        let mut log = ValidationLog::ephemeral();
        assert_eq!(
            log.status(None).state,
            ValidationState::Unvalidated,
            "nothing checked yet"
        );
        assert!(matches!(
            log.record(passing("  ", None)),
            Err(ValidationError::MissingCommand)
        ));
        log.record(passing("cargo test", None)).unwrap();
        let status = log.status(None);
        assert_eq!(status.state, ValidationState::Passed);
        assert!(!status.durable);
    }

    #[test]
    fn a_second_writer_does_not_overwrite_the_first_record() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut first =
            ValidationLog::open(Arc::clone(&store), session, Principal::System).unwrap();
        let mut second =
            ValidationLog::open(Arc::clone(&store), session, Principal::System).unwrap();
        first
            .record(passing("cargo fmt", Some(revision(1))))
            .unwrap();
        let contended = second
            .record(passing("cargo test", Some(revision(1))))
            .unwrap();
        assert_eq!(
            contended.sequence, 2,
            "the late writer follows the early one"
        );

        let rebuilt = ValidationLog::open(store, session, Principal::System).unwrap();
        assert_eq!(rebuilt.records().len(), 2);
        assert_eq!(rebuilt.records()[0].command, "cargo fmt");
        assert_eq!(rebuilt.records()[1].command, "cargo test");
    }
}
