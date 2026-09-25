use crate::sandbox::SandboxPlan;
use arsy_kernel::policy::SandboxAssurance;
use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{OperationId, Principal},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
        OutputSink,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::{self, Read},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, LazyLock, Mutex},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

static CANCELLED: LazyLock<Mutex<HashSet<OperationId>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn cancellation_set() -> &'static Mutex<HashSet<OperationId>> {
    &CANCELLED
}

/// Request cancellation of the currently running process operation.
pub fn cancel(operation: OperationId) {
    if let Ok(mut cancelled) = cancellation_set().lock() {
        cancelled.insert(operation);
    }
}

fn take_cancelled(operation: OperationId) -> bool {
    cancellation_set()
        .lock()
        .map(|mut cancelled| cancelled.remove(&operation))
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct ProcessInput {
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Reaped,
    Terminated,
    Killed,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessResult {
    /// The command that was actually run. `validate.record` binds its claim
    /// of what passed to this, not to a string the caller separately asserts.
    pub argv: Vec<String>,
    pub status_code: Option<i32>,
    pub timed_out: bool,
    pub graceful_termination_sent: bool,
    pub forced_kill_sent: bool,
    pub cleanup: Cleanup,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub sandbox_assurance: SandboxAssurance,
}

pub struct ProcessExecutor {
    contract: OperationContract,
    artifacts: Arc<dyn ArtifactStore>,
    environment: BTreeMap<String, String>,
    grace: Duration,
    retain_until_ms: u64,
    sandbox: Option<SandboxPlan>,
    /// Where the child starts. Without it a command runs wherever the operator
    /// happened to launch ARSY, so `ls` answers a different question than the
    /// workspace the same turn is reading and editing.
    working_directory: Option<PathBuf>,
    output_sink: Mutex<Option<OutputSink>>,
}

impl ProcessExecutor {
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        environment_allowlist: impl IntoIterator<Item = String>,
        grace: Duration,
        retain_until_ms: u64,
    ) -> Self {
        let allowed: BTreeSet<_> = environment_allowlist.into_iter().collect();
        let environment = std::env::vars()
            .filter(|(name, _)| allowed.contains(name))
            .collect();
        Self {
            contract: OperationContract {
                kind: OperationKind::new("process.exec").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([
                        ("argv".into(), JsonType::Array),
                        ("timeout_ms".into(), JsonType::Number),
                        ("max_output_bytes".into(), JsonType::Number),
                    ]),
                    optional: BTreeMap::new(),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::ProcessExec],
                idempotency: Idempotency::Effectful,
                // A command can do anything, including something nothing here
                // can undo.
                reversible: false,
                concurrency: ConcurrencyRule::Parallel,
            },
            artifacts,
            environment,
            grace,
            retain_until_ms,
            sandbox: None,
            working_directory: None,
            output_sink: Mutex::new(None),
        }
    }

    pub fn with_sandbox(mut self, plan: SandboxPlan) -> Self {
        self.sandbox = Some(plan);
        self
    }

    /// Run children in `directory` rather than the process's own cwd.
    pub fn in_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    fn run(
        &self,
        operation: OperationId,
        input: ProcessInput,
        actor: &Principal,
    ) -> Result<OperationOutcome, OperationError> {
        let (program, args) = input
            .argv
            .split_first()
            .ok_or_else(|| OperationError::Schema("argv must not be empty".into()))?;
        if program.is_empty()
            || !(1..=MAX_TIMEOUT_MS).contains(&input.timeout_ms)
            || !(1..=MAX_OUTPUT_BYTES).contains(&input.max_output_bytes)
        {
            return Err(OperationError::Schema(
                "program must be non-empty, timeout_ms at most 86400000, and max_output_bytes at most 16777216".into(),
            ));
        }

        let mut command = if let Some(plan) = &self.sandbox {
            if plan.program != *program || input.max_output_bytes > plan.limits.max_output_bytes {
                return Err(OperationError::Schema(
                    "process request exceeds its sandbox plan".into(),
                ));
            }
            plan.command(program, args)
        } else {
            let mut command = Command::new(program);
            command.args(args);
            command
        };
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        command
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(execution)?;
        let output_sink = self.output_sink.lock().ok().and_then(|sink| sink.clone());
        let stdout = drain(
            child.stdout.take().expect("piped stdout is present"),
            input.max_output_bytes,
            output_sink.clone(),
        );
        let stderr = drain(
            child.stderr.take().expect("piped stderr is present"),
            input.max_output_bytes,
            output_sink,
        );
        let (status, timed_out, graceful, forced, cleanup) = wait_bounded(
            &mut child,
            Duration::from_millis(input.timeout_ms),
            self.grace,
            operation,
        )?;
        let stdout = stdout
            .join()
            .map_err(|_| OperationError::Execution("stdout reader panicked".into()))?
            .map_err(execution)?;
        let stderr = stderr
            .join()
            .map_err(|_| OperationError::Execution("stderr reader panicked".into()))?
            .map_err(execution)?;

        let stdout_artifact = self.put(&stdout.bytes, actor.clone(), "application/octet-stream")?;
        let stderr_artifact = self.put(&stderr.bytes, actor.clone(), "application/octet-stream")?;
        let result = ProcessResult {
            argv: input.argv.clone(),
            status_code: status.code(),
            timed_out,
            graceful_termination_sent: graceful,
            forced_kill_sent: forced,
            cleanup,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            sandbox_assurance: self
                .sandbox
                .as_ref()
                .map_or(SandboxAssurance::None, |plan| plan.assurance),
        };
        let result_artifact = self.put(
            &serde_json::to_vec(&result)
                .map_err(|error| OperationError::Execution(error.to_string()))?,
            actor.clone(),
            "application/json",
        )?;

        Ok(OperationOutcome {
            value: Some(result_artifact.clone()),
            observed_effects: vec![Effect {
                action: CapabilityAction::ProcessExec,
                resource: arsy_kernel::domain::ResourceRef::new("process", program.clone())
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: vec![stdout_artifact, stderr_artifact],
            state: None,
        })
    }

    fn put(
        &self,
        bytes: &[u8],
        creator: Principal,
        media_type: &str,
    ) -> Result<arsy_kernel::domain::ResourceRef, OperationError> {
        self.artifacts
            .put(
                bytes,
                NewArtifact {
                    media_type: media_type.into(),
                    creator,
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map(|metadata| metadata.resource_ref())
            .map_err(|error| OperationError::Execution(error.to_string()))
    }
}

impl OperationExecutor for ProcessExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        if !request
            .requirements
            .iter()
            .any(|requirement| requirement.action == CapabilityAction::ProcessExec)
        {
            return Err(OperationError::Schema(
                "process.exec requires a process.exec capability requirement".into(),
            ));
        }
        let input = serde_json::from_value(request.input.clone())
            .map_err(|error| OperationError::Schema(error.to_string()))?;
        self.run(request.id, input, &request.actor)
    }

    fn set_output_sink(&self, sink: Option<OutputSink>) {
        if let Ok(mut current) = self.output_sink.lock() {
            *current = sink;
        }
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain(
    mut reader: impl Read + Send + 'static,
    limit: u64,
    sink: Option<OutputSink>,
) -> thread::JoinHandle<io::Result<BoundedOutput>> {
    thread::spawn(move || {
        let capacity = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(capacity.min(64 * 1024));
        let mut truncated = false;
        let mut chunk = [0; 8192];
        let mut carry = Vec::new();
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                if let (Some(sink), false) = (&sink, carry.is_empty()) {
                    sink(String::from_utf8_lossy(&carry).into_owned());
                }
                break;
            }
            if let Some(sink) = &sink {
                sink(take_utf8(&mut carry, &chunk[..count]));
            }
            let remaining = capacity.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..count.min(remaining)]);
            truncated |= count > remaining;
        }
        Ok(BoundedOutput { bytes, truncated })
    })
}

/// Decode `carry` then `chunk` for display, holding back a character whose
/// bytes straddle the end of the chunk until the next read completes it.
///
/// A read boundary falls wherever the pipe's buffer did, so decoding each
/// chunk on its own turned a split multibyte character into two `�`.
pub(crate) fn take_utf8(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let keep = incomplete_tail(carry);
    let rest = carry.split_off(carry.len() - keep);
    let text = String::from_utf8_lossy(carry).into_owned();
    *carry = rest;
    text
}

/// How many trailing bytes are the start of a character still missing its
/// continuation bytes.
///
/// Read from the end rather than from `from_utf8`'s first error: an invalid
/// byte earlier in the buffer says nothing about whether the last character
/// is complete, and stopping there turned a split character behind it into
/// `�` too.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

/// Ask a child's process group to stop, then insist once the grace runs out.
///
/// Shared rather than written at each call site: a timeout, a cancellation,
/// and a background session's `process.stop` are the same three steps, and a
/// copy that forgot the forced kill would leave a process behind.
pub(crate) fn end(child: &mut Child, grace: Duration) -> Ended {
    let graceful = terminate(child);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Ended {
                status: Some(status),
                graceful,
                forced: false,
            };
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = force_kill(child);
    Ended {
        status: child.wait().ok(),
        graceful,
        forced: true,
    }
}

/// How a child that had to be stopped actually stopped.
pub(crate) struct Ended {
    pub status: Option<ExitStatus>,
    /// Whether the polite signal was delivered at all.
    pub graceful: bool,
    pub forced: bool,
}

fn wait_bounded(
    child: &mut Child,
    timeout: Duration,
    grace: Duration,
    operation: OperationId,
) -> Result<(ExitStatus, bool, bool, bool, Cleanup), OperationError> {
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        let cancelled = take_cancelled(operation);
        if !cancelled && !timed_out {
            if let Some(status) = child.try_wait().map_err(execution)? {
                return Ok((status, false, false, false, Cleanup::Reaped));
            }
            timed_out = Instant::now() >= deadline;
            if !timed_out {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
        }
        let ended = end(child, grace);
        let status = ended
            .status
            .ok_or_else(|| OperationError::Execution("the child could not be reaped".into()))?;
        let cleanup = if ended.forced {
            Cleanup::Killed
        } else {
            Cleanup::Terminated
        };
        return Ok((status, timed_out, ended.graceful, ended.forced, cleanup));
    }
}

#[cfg(unix)]
fn terminate(child: &Child) -> bool {
    Command::new("kill")
        .args(["-TERM", "--", &format!("-{}", child.id())])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn force_kill(child: &mut Child) -> Result<(), OperationError> {
    let status = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(execution)?;
    if status.success() {
        Ok(())
    } else {
        child.kill().map_err(execution)
    }
}

#[cfg(windows)]
fn terminate(child: &Child) -> bool {
    Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn force_kill(child: &mut Child) -> Result<(), OperationError> {
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &child.id().to_string(), "/T"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(execution)?;
    if status.success() {
        Ok(())
    } else {
        child.kill().map_err(execution)
    }
}

fn execution(error: io::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use arsy_kernel::artifact::{ArtifactReadLimits, FileArtifactStore};

    #[test]
    fn a_character_split_across_reads_is_decoded_whole() {
        let bytes = "héllo 日本".as_bytes();
        let mut carry = Vec::new();
        let mut text = String::new();
        for chunk in bytes.chunks(1) {
            text.push_str(&take_utf8(&mut carry, chunk));
        }
        assert_eq!(text, "héllo 日本");
        assert!(carry.is_empty(), "nothing is left waiting");
    }

    #[test]
    fn an_invalid_byte_is_still_shown_rather_than_held() {
        let mut carry = Vec::new();
        assert_eq!(take_utf8(&mut carry, b"a\xffb"), "a\u{fffd}b");
        assert!(carry.is_empty());
    }

    fn run(argv: Vec<String>, limit: u64, timeout_ms: u64) -> (ProcessResult, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileArtifactStore::open(dir.path(), 0).unwrap());
        let executor =
            ProcessExecutor::new(store.clone(), Vec::new(), Duration::from_millis(20), 0);
        let outcome = executor
            .run(
                OperationId::new(),
                ProcessInput {
                    argv,
                    timeout_ms,
                    max_output_bytes: limit,
                },
                &Principal::System,
            )
            .unwrap();
        let limits = ArtifactReadLimits {
            max_bytes: 4096,
            max_expansion_ratio: 100,
        };
        let result_id = outcome.value.unwrap().value().parse().unwrap();
        let result = serde_json::from_slice(&store.read(result_id, limits).unwrap()).unwrap();
        let stdout_id = outcome.evidence[0].value().parse().unwrap();
        (result, store.read(stdout_id, limits).unwrap())
    }

    #[cfg(unix)]
    #[test]
    fn direct_spawn_bounds_output_and_clears_inherited_environment() {
        let (result, stdout) = run(
            vec![
                "sh".into(),
                "-c".into(),
                "printf %s \"${HOME-unset}\"; printf 123456789".into(),
            ],
            7,
            2_000,
        );

        assert_eq!(stdout, b"unset12");
        assert!(result.stdout_truncated);
        assert_eq!(result.cleanup, Cleanup::Reaped);
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_records_graceful_termination_and_reaps_the_process() {
        let (result, _) = run(
            vec![
                "sh".into(),
                "-c".into(),
                "trap 'exit 0' TERM; while :; do sleep 1; done".into(),
            ],
            16,
            20,
        );

        assert!(result.timed_out);
        assert!(result.graceful_termination_sent);
        assert!(matches!(
            result.cleanup,
            Cleanup::Terminated | Cleanup::Killed
        ));
        assert_eq!(result.forced_kill_sent, result.cleanup == Cleanup::Killed);
    }
}
