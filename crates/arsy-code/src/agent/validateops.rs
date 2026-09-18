//! `validate.*`: the edit → validate → fix loop, as explicit agent progress.
//!
//! A model that runs `pytest` through `process.exec` and reads the output is
//! validating, but nothing durable says so: the transcript can be trimmed,
//! and "did this pass before I claimed done" becomes a question only the
//! model's memory can answer. `validate.record` writes the run to the
//! session's stream through [`arsy_kernel::validation`], so `validate.status`
//! answers it from the record — after a restart as well as during the turn.
//!
//! What is recorded is the run, not the claim: the outcome is read from the
//! artifact the `bash` call itself returned, and the record carries the
//! artifact, a digest of the command, the grants the call held, the task and
//! attempt it belongs to, and the workspace revision it ran against. That last
//! one is what makes a pass expire: evidence is about the tree it ran on, so
//! an edit after a passing check leaves the check visible and no longer
//! current.

use crate::process::ProcessResult;
use arsy_kernel::{
    artifact::{ArtifactReadLimits, ArtifactStore},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{ArtifactId, AttemptId, Principal, ResourceRef, TaskId},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    validation::{NewValidation, ValidationLog, ValidationOutcome},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// How much of an evidence artifact is read to find its exit code. Well past
/// any real `process.exec` result, which is a handful of fields.
const EVIDENCE_LIMITS: ArtifactReadLimits = ArtifactReadLimits {
    max_bytes: 64 * 1024,
    max_expansion_ratio: 1_000,
};

/// Where the turn's validation records are written, and who they belong to.
#[derive(Clone)]
pub struct Validations {
    pub log: Arc<Mutex<ValidationLog>>,
    pub task: Option<TaskId>,
    pub attempt: Option<AttemptId>,
}

impl Validations {
    /// A log with nowhere durable to write, for a caller with no session
    /// stream. Its `validate.status` says `durable: false` rather than
    /// presenting process-local records as if they survived a restart.
    pub fn ephemeral() -> Self {
        Self {
            log: Arc::new(Mutex::new(ValidationLog::ephemeral())),
            task: None,
            attempt: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidateOperation {
    Record,
    Status,
}

impl ValidateOperation {
    pub const ALL: [Self; 2] = [Self::Record, Self::Status];

    const fn kind(self) -> &'static str {
        match self {
            Self::Record => "validate.record",
            Self::Status => "validate.status",
        }
    }

    const fn idempotency(self) -> Idempotency {
        match self {
            Self::Record => Idempotency::Effectful,
            Self::Status => Idempotency::Idempotent,
        }
    }

    /// A read cannot conflict with another read; recording an outcome
    /// mutates the log and has to see what a previous record left.
    const fn concurrency(self) -> ConcurrencyRule {
        match self {
            Self::Status => ConcurrencyRule::Parallel,
            Self::Record => ConcurrencyRule::ExclusiveGlobal,
        }
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let (required, optional) = match self {
            Self::Record => (
                vec![string("command"), string("evidence")],
                vec![string("detail")],
            ),
            Self::Status => (Vec::new(), Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

pub struct ValidateExecutor {
    operation: ValidateOperation,
    contract: OperationContract,
    validations: Validations,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    workspace_root: PathBuf,
}

impl ValidateExecutor {
    pub fn executors(
        validations: &Validations,
        artifacts: &Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
        workspace_root: &Path,
    ) -> Vec<Arc<dyn OperationExecutor>> {
        ValidateOperation::ALL
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
                        reversible: true,
                        concurrency: operation.concurrency(),
                    },
                    validations: validations.clone(),
                    artifacts: Arc::clone(artifacts),
                    retain_until_ms,
                    workspace_root: workspace_root.to_path_buf(),
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

    /// The outcome a `bash` call actually had, read from the artifact its own
    /// call returned — not asserted by whoever is calling `validate.record`.
    /// A model that never ran the command, or ran a different one, has no
    /// artifact id to give; one that ran it and got a failure cannot report
    /// `passed` by simply saying so.
    ///
    /// The evidence proves only that *some* command exited 0 unless it is
    /// also checked against `command`: `bash` runs everything as `sh -c
    /// <command>`, so its argv's last element is the command text, and that
    /// has to match what is being recorded — otherwise `true` is evidence
    /// for `cargo test`.
    fn outcome_of(
        &self,
        evidence: &str,
        command: &str,
    ) -> Result<(ArtifactId, ValidationOutcome), OperationError> {
        let id: ArtifactId = evidence
            .parse()
            .map_err(|_| OperationError::Execution("`evidence` is not an artifact id".into()))?;
        let bytes = self
            .artifacts
            .read(id, EVIDENCE_LIMITS)
            .map_err(|error| OperationError::Execution(format!("evidence artifact: {error}")))?;
        let result: ProcessResult = serde_json::from_slice(&bytes).map_err(|_| {
            OperationError::Execution(
                "evidence must be the artifact id a `bash` call returned".into(),
            )
        })?;
        if result.argv.last().map(String::as_str) != Some(command) {
            return Err(OperationError::Execution(
                "`evidence` is a result for a different command than `command` names".into(),
            ));
        }
        Ok((
            id,
            if !result.timed_out && result.status_code == Some(0) {
                ValidationOutcome::Passed
            } else {
                ValidationOutcome::Failed
            },
        ))
    }
}

impl OperationExecutor for ValidateExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        // Read once per call, and used both for the record being written and
        // for judging the ones already there: a status answered against a
        // different revision than it was asked about would be the bug this
        // field exists to prevent.
        let revision = crate::git::revision(&self.workspace_root);
        let mut log = self
            .validations
            .log
            .lock()
            .map_err(|_| OperationError::Execution("validation state poisoned".into()))?;

        if self.operation == ValidateOperation::Record {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (artifact, outcome) = self.outcome_of(
                input
                    .get("evidence")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                command,
            )?;
            log.record(NewValidation {
                operation: self.contract.kind.as_str().to_owned(),
                command: command.to_owned(),
                artifact,
                task: self.validations.task,
                attempt: self.validations.attempt,
                grants: grants.iter().map(|grant| grant.id).collect(),
                workspace_revision: revision,
                outcome,
                detail: input
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                recorded_at_ms: arsy_kernel::artifact::unix_time_ms(),
            })
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        }

        let status = log.status(revision);
        drop(log);
        let value = self.put(&status, request.actor.clone())?;

        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::SystemModify,
                resource: ResourceRef::new("system", "validation")
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
        domain::{OperationId, Principal, SessionId},
        event::{EventStore, MemoryEventStore},
        validation::{ValidationState, ValidationStatus},
    };

    fn setup() -> (
        tempfile::TempDir,
        Vec<Arc<dyn OperationExecutor>>,
        Arc<dyn ArtifactStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let executors =
            ValidateExecutor::executors(&Validations::ephemeral(), &artifacts, 0, dir.path());
        (dir, executors, artifacts)
    }

    /// The artifact a `bash` call itself would have left, so a test can hand
    /// `validate.record` a real id rather than a claim.
    fn evidence(
        artifacts: &Arc<dyn ArtifactStore>,
        command: &str,
        status_code: Option<i32>,
    ) -> String {
        let result = ProcessResult {
            argv: vec!["sh".into(), "-c".into(), command.into()],
            status_code,
            timed_out: false,
            graceful_termination_sent: false,
            forced_kill_sent: false,
            cleanup: crate::process::Cleanup::Reaped,
            stdout_truncated: false,
            stderr_truncated: false,
            sandbox_assurance: arsy_kernel::policy::SandboxAssurance::None,
        };
        let reference = super::super::store(
            artifacts.as_ref(),
            &result,
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        reference.value().to_owned()
    }

    fn call(
        executors: &[Arc<dyn OperationExecutor>],
        artifacts: &Arc<dyn ArtifactStore>,
        kind: &str,
        input: Value,
    ) -> ValidationStatus {
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
        let outcome = executor.execute(&request, &[]).unwrap();
        let reference = outcome.value.expect("validate calls always return a log");
        let id: ArtifactId = reference.value().parse().unwrap();
        let bytes = artifacts
            .read(
                id,
                ArtifactReadLimits {
                    max_bytes: 1024 * 1024,
                    max_expansion_ratio: 1_000,
                },
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_failing_run_leaves_the_task_actionable_and_a_passing_one_does_not() {
        let (_dir, executors, artifacts) = setup();

        let status = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({
                "command": "cargo test -p arsy-code",
                "evidence": evidence(&artifacts, "cargo test -p arsy-code", Some(1)),
                "detail": "assertion failed: left == right"
            }),
        );
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.state, ValidationState::Failed);
        assert!(status.actionable, "a failing check leaves work to do");

        let status = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({
                "command": "cargo test -p arsy-code",
                "evidence": evidence(&artifacts, "cargo test -p arsy-code", Some(0)),
            }),
        );
        assert_eq!(status.records.len(), 2);
        assert!(
            !status.actionable,
            "the most recent, passing run is what completion cites"
        );

        let read = call(
            &executors,
            &artifacts,
            "validate.status",
            serde_json::json!({}),
        );
        assert_eq!(
            read, status,
            "status reads the same log without mutating it"
        );
    }

    /// The record is the evidence a later process reads, so it has to name
    /// what was checked and survive the process that wrote it.
    #[test]
    fn a_recorded_check_carries_its_lineage_and_outlives_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let task = TaskId::new();
        let attempt = AttemptId::new();
        let validations = Validations {
            log: Arc::new(Mutex::new(
                ValidationLog::open(Arc::clone(&store), session, Principal::System).unwrap(),
            )),
            task: Some(task),
            attempt: Some(attempt),
        };
        let executors = ValidateExecutor::executors(&validations, &artifacts, 0, dir.path());
        let status = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({
                "command": "cargo test",
                "evidence": evidence(&artifacts, "cargo test", Some(0)),
            }),
        );
        let record = &status.records[0];
        assert_eq!(record.task, Some(task));
        assert_eq!(record.attempt, Some(attempt));
        assert_eq!(record.operation, "validate.record");
        assert_eq!(
            record.command_digest,
            arsy_kernel::validation::command_digest("cargo test")
        );
        assert!(status.durable);

        let rebuilt = ValidationLog::open(store, session, Principal::System).unwrap();
        assert_eq!(rebuilt.records(), status.records.as_slice());
    }

    /// A model cannot claim a passing check that never ran: there is no
    /// evidence artifact for a command it did not call `bash` for, and an id
    /// it makes up does not resolve.
    #[test]
    fn a_claim_with_no_real_evidence_artifact_is_rejected() {
        let (_dir, executors, _artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "validate.record")
            .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({"command": "make test", "evidence": "not-an-artifact-id"}),
        };
        assert!(executor.execute(&request, &[]).is_err());
    }

    /// Evidence that resolves but is not a `bash` result — someone else's
    /// artifact, or the plan snapshot from `plan.add` — is rejected the same
    /// way: a real id is necessary but has to name the right kind of thing.
    #[test]
    fn evidence_that_is_not_a_command_result_is_rejected() {
        let (_dir, executors, artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "validate.record")
            .unwrap();
        let not_a_process_result = super::super::store(
            artifacts.as_ref(),
            &serde_json::json!({"steps": []}),
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({
                "command": "make test",
                "evidence": not_a_process_result.value(),
            }),
        };
        assert!(executor.execute(&request, &[]).is_err());
    }

    /// A command that timed out is not a pass, whatever its exit code reads
    /// as by the time the process was killed.
    #[test]
    fn a_timed_out_run_is_never_a_pass() {
        let (_dir, executors, artifacts) = setup();
        let timed_out = ProcessResult {
            argv: vec!["sh".into(), "-c".into(), "sleep 300".into()],
            status_code: Some(0),
            timed_out: true,
            graceful_termination_sent: true,
            forced_kill_sent: false,
            cleanup: crate::process::Cleanup::Terminated,
            stdout_truncated: false,
            stderr_truncated: false,
            sandbox_assurance: arsy_kernel::policy::SandboxAssurance::None,
        };
        let reference = super::super::store(
            artifacts.as_ref(),
            &timed_out,
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        let status = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({"command": "sleep 300", "evidence": reference.value()}),
        );
        assert!(
            status.actionable,
            "a timed-out run is not evidence of a pass"
        );
    }

    /// Evidence that `true` exited 0 is not evidence that `cargo test`
    /// passed: the command named has to be the one the evidence itself ran.
    #[test]
    fn evidence_for_a_different_command_is_rejected() {
        let (_dir, executors, artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "validate.record")
            .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({
                "command": "cargo test -p arsy-code",
                "evidence": evidence(&artifacts, "true", Some(0)),
            }),
        };
        assert!(executor.execute(&request, &[]).is_err());
    }
}
