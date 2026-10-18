use crate::resource::Workspace;
use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef, StateVersion, WorkspaceVersion},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    policy::WorkspaceCleanliness,
};
#[cfg(unix)]
use cap_std::fs::PermissionsExt;
use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_LOG_ENTRIES: u64 = 1_000;

/// Whether the working tree has uncommitted changes, or `None` when that
/// cannot be established — Git is absent, or this is not a repository.
///
/// Policy raises a Git mutation to approval in a dirty tree, so a caller that
/// wants a truthful decision needs this before it evaluates. It is a read-only
/// `git status`, which is why it does not go through the operation registry:
/// asking for a grant in order to explain a grant would not terminate.
pub fn cleanliness(workspace: &Path) -> Option<WorkspaceCleanliness> {
    let output = configured_git()
        .args([
            "-c",
            "core.fsmonitor=false",
            "--no-pager",
            "status",
            "--porcelain",
        ])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(if output.stdout.is_empty() {
        WorkspaceCleanliness::Clean
    } else {
        WorkspaceCleanliness::Dirty
    })
}

/// The commit a tree is on, as its full object ID.
///
/// Read-only and outside the operation registry, for the same reason as
/// [`cleanliness`]: an integrator has to name the revision it is applying
/// before it can ask for authority to apply it.
pub fn head_revision(workspace: &Path) -> Result<String, io::Error> {
    plumbing(workspace, &["--no-pager", "rev-parse", "HEAD"]).map(|output| output.trim().to_owned())
}

/// The files that differ between two revisions, as Git reports them.
///
/// What a writer actually changed, rather than what it says it changed: an
/// integrator checking for overlap needs the first.
pub fn changed_between(workspace: &Path, base: &str, head: &str) -> Result<Vec<String>, io::Error> {
    Ok(plumbing(
        workspace,
        &[
            "--no-pager",
            "diff",
            "--name-only",
            base,
            head,
            "--",
            ".",
            // ARSY's own state, which it writes into every workspace it runs
            // in. Reporting it as a change the writer made would put the
            // harness's bookkeeping into an integrator's overlap check.
            ":(exclude).arsy/state/**",
        ],
    )?
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(str::to_owned)
    .collect())
}

fn configured_git() -> Command {
    let mut command = Command::new("git");
    command
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn plumbing(workspace: &Path, arguments: &[&str]) -> Result<String, io::Error> {
    let output = configured_git()
        .args(["-c", "core.fsmonitor=false"])
        .args(arguments)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

/// What the workspace currently is, as one digest — or `None` when Git cannot
/// say, which is not a match with anything.
///
/// Evidence is only evidence about the tree it ran against. `HEAD` and
/// porcelain status identify changed paths, but not subsequent edits to the
/// same dirty file. Hash the tracked diff and untracked file contents too.
///
/// Read-only, and for the same reason as [`cleanliness`] it does not go
/// through the operation registry.
pub fn revision(workspace: &Path) -> Option<WorkspaceVersion> {
    let git = |arguments: &[&str]| {
        let output = configured_git()
            .args(["-c", "core.fsmonitor=false"])
            .args(arguments)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    };
    let head = git(&["--no-pager", "rev-parse", "HEAD"])?;
    let status = git(&["--no-pager", "status", "--porcelain"])?;
    let untracked = git(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
        ".",
        ":(exclude).arsy/state/**",
    ])?;
    let mut digest = Sha256::new();
    digest.update(&head);
    digest.update([0]);
    digest.update(&status);
    digest.update([0]);

    let mut diff = Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "HEAD",
        ])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let read = hash_content(diff.stdout.as_mut()?, &mut digest);
    let exited = diff.wait().ok()?;
    read?;
    if !exited.success() {
        return None;
    }
    if !untracked.is_empty() {
        let directory = Dir::open_ambient_dir(workspace, ambient_authority()).ok()?;
        for name in untracked
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let (marker, content) = view_entry_fingerprint(&directory, workspace, name).ok()??;
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name);
            digest.update([marker]);
            digest.update(content);
        }
    }
    Some(WorkspaceVersion(StateVersion::from_digest(
        digest.finalize().into(),
    )))
}
/// The contents and executable modes `git add --all` will consider: tracked
/// and nonignored untracked paths. Ignore build outputs Git would not commit,
/// but never trust index contents, HEAD, mtimes, or status shortcuts for the
/// bytes themselves. Symlinks are hashed as links, never followed out of view.
pub fn content_fingerprint(workspace: &Path) -> io::Result<StateVersion> {
    if std::fs::symlink_metadata(workspace)?
        .file_type()
        .is_symlink()
    {
        return Err(io::Error::other("writer view is a symlink"));
    }
    let directory = Dir::open_ambient_dir(workspace, ambient_authority())?;
    let workspace_text = workspace
        .to_str()
        .ok_or_else(|| io::Error::other("invalid workspace path"))?;
    let output = configured_git()
        .args([
            "-c",
            "core.fsmonitor=false",
            "-C",
            workspace_text,
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            ":(exclude).arsy/state/**",
        ])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let mut entries: BTreeMap<Vec<u8>, (u8, [u8; 32])> = BTreeMap::new();
    for name in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        if let Some(entry) = view_entry_fingerprint(&directory, workspace, name)? {
            entries.insert(name.to_vec(), entry);
        }
    }
    fingerprint_entries(entries)
}

/// Check every parent component without following symlinks, then hash only
/// the final path Git would stage. A tracked deletion has no bytes to hash.
fn view_entry_fingerprint(
    directory: &Dir,
    workspace: &Path,
    name: &[u8],
) -> io::Result<Option<(u8, [u8; 32])>> {
    #[cfg(unix)]
    let relative = {
        use std::os::unix::ffi::OsStrExt;
        Path::new(std::ffi::OsStr::from_bytes(name))
    };
    #[cfg(not(unix))]
    let relative = Path::new(std::str::from_utf8(name).map_err(io::Error::other)?);
    let mut path = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(part) = component else {
            return Err(io::Error::other("invalid writer file path"));
        };
        path.push(part);
        let metadata = match directory.symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if components.peek().is_some() {
            if !metadata.is_dir() {
                return Err(io::Error::other(
                    "writer path crosses a symlink or non-directory",
                ));
            }
            continue;
        }
        let mut digest = Sha256::new();
        let marker = if metadata.file_type().is_symlink() {
            digest.update(path_bytes(&directory.read_link(&path)?)?);
            b'l'
        } else if metadata.is_file() {
            hash_content(&mut directory.open(&path)?, &mut digest)
                .ok_or_else(|| io::Error::other("cannot hash writer file"))?;
            #[cfg(unix)]
            {
                if metadata.permissions().mode() & 0o111 != 0 {
                    b'x'
                } else {
                    b'f'
                }
            }
            #[cfg(not(unix))]
            {
                b'f'
            }
        } else {
            return Err(io::Error::other(format!(
                "unsupported writer file: {}",
                workspace.join(&path).display()
            )));
        };
        return Ok(Some((marker, digest.finalize().into())));
    }
    Err(io::Error::other("invalid writer file path"))
}

/// The bytes that Git actually committed, not an index or working-tree guess.
/// Blob contents are read through Git; symlink targets never resolve on disk.
pub fn committed_content_fingerprint(workspace: &Path, revision: &str) -> io::Result<StateVersion> {
    let workspace = workspace
        .to_str()
        .ok_or_else(|| io::Error::other("invalid workspace path"))?;
    let output = Command::new("git")
        .args([
            "-C",
            workspace,
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            revision,
        ])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let mut entries = BTreeMap::new();
    let mut child = Command::new("git")
        .args(["-C", workspace, "cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing Git input"))?;
    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing Git output"))?,
    );
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let mut parts = entry.splitn(2, |byte| *byte == b'\t');
        let header = parts
            .next()
            .ok_or_else(|| io::Error::other("invalid Git tree entry"))?;
        let name = parts
            .next()
            .ok_or_else(|| io::Error::other("invalid Git tree entry"))?;
        if name == b".git" || name.starts_with(b".git/") || name.starts_with(b".arsy/state/") {
            continue;
        }
        let mut fields = header.split(|byte| *byte == b' ');
        let mode = fields
            .next()
            .ok_or_else(|| io::Error::other("missing Git mode"))?;
        let kind = fields
            .next()
            .ok_or_else(|| io::Error::other("missing Git kind"))?;
        let oid = fields
            .next()
            .ok_or_else(|| io::Error::other("missing Git object"))?;
        if kind != b"blob" || fields.next().is_some() || !oid.iter().all(u8::is_ascii_hexdigit) {
            return Err(io::Error::other("invalid Git blob entry"));
        }
        let marker = if mode == b"120000" {
            b'l'
        } else if mode == b"100644" {
            b'f'
        } else if mode == b"100755" {
            b'x'
        } else {
            return Err(io::Error::other("unsupported Git tree entry"));
        };
        let oid = std::str::from_utf8(oid).map_err(io::Error::other)?;
        stdin.write_all(oid.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        let mut header = String::new();
        stdout.read_line(&mut header)?;
        let mut fields = header.split_ascii_whitespace();
        if fields.next() != Some(oid) || fields.next() != Some("blob") {
            return Err(io::Error::other("unexpected Git blob header"));
        }
        let size: u64 = fields
            .next()
            .ok_or_else(|| io::Error::other("missing Git blob size"))?
            .parse()
            .map_err(io::Error::other)?;
        if fields.next().is_some() {
            return Err(io::Error::other("invalid Git blob header"));
        }
        let mut bytes = (&mut stdout).take(size);
        let mut digest = Sha256::new();
        hash_content(&mut bytes, &mut digest)
            .ok_or_else(|| io::Error::other("cannot hash committed blob"))?;
        if bytes.limit() != 0 {
            return Err(io::Error::other("truncated committed blob"));
        }
        let mut newline = [0u8; 1];
        stdout.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(io::Error::other("invalid committed blob separator"));
        }
        entries.insert(name.to_vec(), (marker, digest.finalize().into()));
    }
    drop(stdin);
    drop(stdout);
    if !child.wait()?.success() {
        return Err(io::Error::other("cannot read committed blobs"));
    }
    fingerprint_entries(entries)
}

fn fingerprint_entries(entries: BTreeMap<Vec<u8>, (u8, [u8; 32])>) -> io::Result<StateVersion> {
    let mut digest = Sha256::new();
    for (name, (marker, content)) in entries {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(&name);
        digest.update([marker]);
        digest.update(content);
    }
    Ok(StateVersion::from_digest(digest.finalize().into()))
}

fn path_bytes(path: &Path) -> io::Result<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        Ok(path
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF-8 path"))?
            .as_bytes())
    }
}

fn hash_content(reader: &mut impl Read, digest: &mut Sha256) -> Option<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = reader.read(&mut buffer).ok()?;
        if size == 0 {
            return Some(());
        }
        digest.update(&buffer[..size]);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitOperation {
    Status,
    Diff,
    Log,
    Blame,
    Branch,
}

impl GitOperation {
    pub const ALL: [Self; 5] = [
        Self::Status,
        Self::Diff,
        Self::Log,
        Self::Blame,
        Self::Branch,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Status => "git.status",
            Self::Diff => "git.diff",
            Self::Log => "git.log",
            Self::Blame => "git.blame",
            Self::Branch => "git.branch",
        }
    }
}

/// One entry from `git status --porcelain=v1`, decoded rather than left as a
/// line the model has to parse itself.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedFile {
    /// The two-letter index/worktree status code, e.g. `" M"`, `"??"`, `"A "`.
    pub status: String,
    /// The current path. For a rename this is the path after `-> `; the
    /// porcelain line itself is kept in `output` for the rare caller that
    /// needs the old path too.
    pub path: String,
}

/// Porcelain v1 is `XY PATH` or, for a rename/copy, `XY OLD -> NEW`. Neither
/// side is quoted unless it contains a character `core.quotepath` would
/// otherwise mangle, which callers of this tool do not need decoded.
fn parse_status(porcelain: &str) -> Vec<ChangedFile> {
    porcelain
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| {
            let status = line[..2].to_owned();
            let rest = &line[3..];
            // The arrow only marks a rename or copy; an ordinary path is
            // reported byte for byte, including one that happens to contain
            // the substring " -> ".
            let path = if status.starts_with(['R', 'C']) {
                rest.rsplit_once(" -> ").map_or(rest, |(_, new)| new)
            } else {
                rest
            };
            ChangedFile {
                status,
                path: path.to_owned(),
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffInput {
    revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogInput {
    max_entries: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlameInput {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitResult {
    pub operation: String,
    pub output: String,
    pub workspace: Option<WorkspaceCleanliness>,
    /// Parsed from `output` for `git.status`; empty for every other kind.
    pub changed_files: Vec<ChangedFile>,
}

pub struct GitExecutor {
    operation: GitOperation,
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl GitExecutor {
    pub fn new(
        operation: GitOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        let required = match operation {
            GitOperation::Status => BTreeMap::new(),
            GitOperation::Diff => BTreeMap::from([("revision".into(), JsonType::String)]),
            GitOperation::Log => BTreeMap::from([("max_entries".into(), JsonType::Number)]),
            GitOperation::Blame => BTreeMap::from([("path".into(), JsonType::String)]),
            GitOperation::Branch => BTreeMap::new(),
        };
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required,
                    optional: BTreeMap::new(),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::GitRead],
                idempotency: Idempotency::Idempotent,
                // Reading history changes nothing.
                reversible: true,
                concurrency: ConcurrencyRule::Parallel,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }

    fn arguments(&self, input: &serde_json::Value) -> Result<Vec<String>, OperationError> {
        match self.operation {
            GitOperation::Status => {
                serde_json::from_value::<EmptyInput>(input.clone()).map_err(schema)?;
                Ok(vec!["status".into(), "--porcelain=v1".into()])
            }
            GitOperation::Diff => {
                let input: DiffInput = serde_json::from_value(input.clone()).map_err(schema)?;
                if input.revision.is_empty()
                    || input.revision.len() > 256
                    || input.revision.starts_with('-')
                    || !input
                        .revision
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"/._~^{}".contains(&byte))
                {
                    return Err(OperationError::Schema("invalid Git revision".into()));
                }
                Ok(vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    input.revision,
                    "--".into(),
                ])
            }
            GitOperation::Log => {
                let input: LogInput = serde_json::from_value(input.clone()).map_err(schema)?;
                if !(1..=MAX_LOG_ENTRIES).contains(&input.max_entries) {
                    return Err(OperationError::Schema(
                        "max_entries must be between 1 and 1000".into(),
                    ));
                }
                Ok(vec![
                    "log".into(),
                    format!("--max-count={}", input.max_entries),
                    "--format=%H%x09%an%x09%aI%x09%s".into(),
                ])
            }
            GitOperation::Blame => {
                let input: BlameInput = serde_json::from_value(input.clone()).map_err(schema)?;
                let path = confined(&input.path)?;
                Ok(vec![
                    "blame".into(),
                    "--line-porcelain".into(),
                    "--".into(),
                    path,
                ])
            }
            GitOperation::Branch => {
                serde_json::from_value::<EmptyInput>(input.clone()).map_err(schema)?;
                Ok(vec!["branch".into(), "--show-current".into()])
            }
        }
    }

    fn run(
        &self,
        input: &serde_json::Value,
        actor: &Principal,
    ) -> Result<OperationOutcome, OperationError> {
        let args = self.arguments(input)?;
        let mut child = Command::new("git")
            .args([
                "--no-pager",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=true",
            ])
            .args(&args)
            .current_dir(&self.workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(execution)?;
        let stdout = drain(child.stdout.take().expect("stdout is piped"));
        let stderr = drain(child.stderr.take().expect("stderr is piped"));
        let status = child.wait().map_err(execution)?;
        let stdout = stdout.join().map_err(|_| panicked())?.map_err(execution)?;
        let stderr = stderr.join().map_err(|_| panicked())?.map_err(execution)?;
        if !status.success() {
            return Err(OperationError::Execution(
                String::from_utf8_lossy(&stderr).into_owned(),
            ));
        }
        let output = String::from_utf8(stdout)
            .map_err(|_| OperationError::Execution("Git output is not UTF-8".into()))?;
        let changed_files = if self.operation == GitOperation::Status {
            parse_status(&output)
        } else {
            Vec::new()
        };
        let result = GitResult {
            operation: self.operation.kind().into(),
            workspace: (self.operation == GitOperation::Status).then_some(if output.is_empty() {
                WorkspaceCleanliness::Clean
            } else {
                WorkspaceCleanliness::Dirty
            }),
            output,
            changed_files,
        };
        let metadata = self
            .artifacts
            .put(
                &serde_json::to_vec(&result)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
                NewArtifact {
                    media_type: "application/json".into(),
                    creator: actor.clone(),
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let evidence = metadata.resource_ref();
        Ok(OperationOutcome {
            value: Some(evidence.clone()),
            observed_effects: vec![Effect {
                action: CapabilityAction::GitRead,
                resource: ResourceRef::new("git", ".")
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: vec![evidence],
            state: None,
        })
    }
}

impl OperationExecutor for GitExecutor {
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
            .any(|requirement| requirement.action == CapabilityAction::GitRead)
        {
            return Err(OperationError::Schema(
                "Git read operations require a git.read capability".into(),
            ));
        }
        self.run(&request.input, &request.actor)
    }
}

fn confined(path: &str) -> Result<String, OperationError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(OperationError::Schema(
            "path must stay within the workspace".into(),
        ));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| OperationError::Schema("path must be UTF-8".into()))
}

fn drain(stream: impl Read + Send + 'static) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut stream = stream;
        let mut bytes = Vec::new();
        let mut overflow = false;
        let mut chunk = [0; 8192];
        loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..count.min(remaining)]);
            overflow |= count > remaining;
        }
        if overflow {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Git output exceeds 1048576 bytes",
            ));
        }
        Ok(bytes)
    })
}

fn schema(error: serde_json::Error) -> OperationError {
    OperationError::Schema(error.to_string())
}

fn execution(error: io::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}

fn panicked() -> OperationError {
    OperationError::Execution("Git output reader panicked".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactReadLimits, FileArtifactStore},
        domain::{ArtifactId, OperationId},
    };
    use serde_json::json;
    use std::{process::Command, str::FromStr};

    #[test]
    fn typed_reads_report_cleanliness_and_attach_evidence() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        std::fs::write(temp.path().join("tracked.txt"), "second\n").unwrap();

        let workspace = Workspace::open(temp.path()).unwrap();
        let artifacts =
            Arc::new(FileArtifactStore::open(temp.path().join("artifacts"), 0).unwrap());
        let cases = [
            (GitOperation::Status, json!({})),
            (GitOperation::Diff, json!({"revision": "HEAD"})),
            (GitOperation::Log, json!({"max_entries": 1})),
            (GitOperation::Blame, json!({"path": "tracked.txt"})),
            (GitOperation::Branch, json!({})),
        ];

        for (operation, input) in cases {
            let executor = GitExecutor::new(operation, &workspace, artifacts.clone(), 0);
            assert_eq!(executor.contract().actions, [CapabilityAction::GitRead]);
            let outcome = executor
                .execute(
                    &OperationRequest {
                        id: OperationId::new(),
                        kind: executor.contract().kind.clone(),
                        actor: Principal::System,
                        requirements: vec![arsy_kernel::capability::CapabilityRequirement::new(
                            CapabilityAction::GitRead,
                            ResourceRef::new("git", ".").unwrap(),
                        )],
                        input,
                    },
                    &[],
                )
                .unwrap();
            assert_eq!(outcome.value.as_ref(), outcome.evidence.first());
            let id = ArtifactId::from_str(outcome.evidence[0].value()).unwrap();
            let bytes = artifacts
                .read(
                    id,
                    ArtifactReadLimits {
                        max_bytes: MAX_GIT_OUTPUT_BYTES as u64,
                        max_expansion_ratio: 100,
                    },
                )
                .unwrap();
            let result: GitResult = serde_json::from_slice(&bytes).unwrap();
            assert!(!result.output.is_empty());
            if operation == GitOperation::Status {
                assert_eq!(result.workspace, Some(WorkspaceCleanliness::Dirty));
            }
        }

        assert!(
            GitExecutor::new(GitOperation::Blame, &workspace, artifacts, 0)
                .arguments(&json!({"path": "../outside"}))
                .is_err()
        );
    }

    #[test]
    fn revision_detects_content_changes_without_a_porcelain_status_change() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        let tracked = temp.path().join("tracked.txt");
        let untracked = temp.path().join("untracked.txt");
        std::fs::write(&tracked, "committed").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);

        std::fs::write(&tracked, "first edit").unwrap();
        let before = revision(temp.path()).unwrap();
        std::fs::write(&tracked, "second edit").unwrap();
        let after = revision(temp.path()).unwrap();
        assert_ne!(before, after, "the same dirty path can hold different code");

        std::fs::write(&untracked, "first version").unwrap();
        let before = revision(temp.path()).unwrap();
        std::fs::write(&untracked, "second version").unwrap();
        let after = revision(temp.path()).unwrap();
        assert_ne!(
            before, after,
            "untracked content can change without a new path"
        );
        assert_eq!(after, revision(temp.path()).unwrap());
    }

    #[test]
    fn committed_tree_must_contain_exactly_the_checked_bytes() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        let file = temp.path().join("tracked.txt");
        std::fs::write(&file, "base").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "base"]);

        std::fs::write(&file, "staged old bytes").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        std::fs::write(&file, "checked new bytes").unwrap();
        let checked = content_fingerprint(temp.path()).unwrap();
        git(temp.path(), &["commit", "-qm", "stale staged content"]);
        let head = head_revision(temp.path()).unwrap();
        assert_ne!(
            checked,
            committed_content_fingerprint(temp.path(), &head).unwrap()
        );

        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "matching content"]);
        let head = head_revision(temp.path()).unwrap();
        assert_eq!(
            content_fingerprint(temp.path()).unwrap(),
            committed_content_fingerprint(temp.path(), &head).unwrap()
        );
        std::fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
        git(temp.path(), &["add", ".gitignore"]);
        git(temp.path(), &["commit", "-qm", "ignore build output"]);
        let head = head_revision(temp.path()).unwrap();
        let before = content_fingerprint(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("target/build-output"), "ignored").unwrap();
        std::fs::create_dir_all(temp.path().join(".arsy/state")).unwrap();
        std::fs::write(temp.path().join(".arsy/state/session"), "harness state").unwrap();
        assert_eq!(before, content_fingerprint(temp.path()).unwrap());
        std::fs::write(temp.path().join("unexpected.txt"), "not committed").unwrap();
        assert_ne!(
            content_fingerprint(temp.path()).unwrap(),
            committed_content_fingerprint(temp.path(), &head).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_must_match_the_committed_tree() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        git(temp.path(), &["config", "core.filemode", "true"]);
        let script = temp.path().join("script.sh");
        std::fs::write(&script, "printf safe").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        git(temp.path(), &["add", "script.sh"]);
        git(temp.path(), &["commit", "-qm", "base"]);
        let head = head_revision(temp.path()).unwrap();
        assert_eq!(
            content_fingerprint(temp.path()).unwrap(),
            committed_content_fingerprint(temp.path(), &head).unwrap()
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(
            content_fingerprint(temp.path()).unwrap(),
            committed_content_fingerprint(temp.path(), &head).unwrap()
        );
        git(temp.path(), &["add", "script.sh"]);
        git(temp.path(), &["commit", "-qm", "mark executable"]);
        assert_eq!(
            content_fingerprint(temp.path()).unwrap(),
            committed_content_fingerprint(temp.path(), &head_revision(temp.path()).unwrap())
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn content_fingerprint_hashes_symlink_target_without_following_ignored_file() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        std::fs::write(temp.path().join(".gitignore"), "ignored*.txt\n").unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "original").unwrap();
        symlink("ignored.txt", temp.path().join("link")).unwrap();
        git(temp.path(), &["add", ".gitignore", "link"]);

        let before = content_fingerprint(temp.path()).unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "changed ignored content").unwrap();
        assert_eq!(before, content_fingerprint(temp.path()).unwrap());
        std::fs::remove_file(temp.path().join("link")).unwrap();
        symlink("ignored-next.txt", temp.path().join("link")).unwrap();
        assert_ne!(before, content_fingerprint(temp.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_refuses_tracked_paths_through_an_external_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/file"), "original").unwrap();
        git(temp.path(), &["add", "src/file"]);
        std::fs::remove_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(outside.path().join("file"), "private").unwrap();
        symlink(outside.path(), temp.path().join("src")).unwrap();
        assert!(content_fingerprint(temp.path()).is_err());
    }

    /// A dirty tree with a modification, an addition, and an untracked file
    /// produces a structured entry per file, not a porcelain line the caller
    /// has to parse itself.
    #[test]
    fn status_on_a_dirty_tree_is_parsed_into_changed_files() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);

        std::fs::write(temp.path().join("tracked.txt"), "second\n").unwrap();
        std::fs::write(temp.path().join("staged.txt"), "new\n").unwrap();
        git(temp.path(), &["add", "staged.txt"]);
        std::fs::write(temp.path().join("untracked.txt"), "new\n").unwrap();

        let workspace = Workspace::open(temp.path()).unwrap();
        let artifacts =
            Arc::new(FileArtifactStore::open(temp.path().join("artifacts"), 0).unwrap());
        let executor = GitExecutor::new(GitOperation::Status, &workspace, artifacts.clone(), 0);
        let outcome = executor
            .execute(
                &OperationRequest {
                    id: OperationId::new(),
                    kind: executor.contract().kind.clone(),
                    actor: Principal::System,
                    requirements: vec![arsy_kernel::capability::CapabilityRequirement::new(
                        CapabilityAction::GitRead,
                        ResourceRef::new("git", ".").unwrap(),
                    )],
                    input: json!({}),
                },
                &[],
            )
            .unwrap();
        let id = ArtifactId::from_str(outcome.evidence[0].value()).unwrap();
        let bytes = artifacts
            .read(
                id,
                ArtifactReadLimits {
                    max_bytes: MAX_GIT_OUTPUT_BYTES as u64,
                    max_expansion_ratio: 100,
                },
            )
            .unwrap();
        let result: GitResult = serde_json::from_slice(&bytes).unwrap();
        let mut by_path: std::collections::BTreeMap<&str, &str> = result
            .changed_files
            .iter()
            .map(|entry| (entry.path.as_str(), entry.status.as_str()))
            .collect();
        assert_eq!(by_path.remove("tracked.txt"), Some(" M"));
        assert_eq!(by_path.remove("staged.txt"), Some("A "));
        assert_eq!(by_path.remove("untracked.txt"), Some("??"));
        assert!(by_path.is_empty(), "{by_path:?}");
    }

    /// An untracked or modified file whose name happens to contain " -> " is
    /// not a rename: the arrow is only ever a separator for the `R`/`C`
    /// status codes porcelain actually uses it for.
    #[test]
    fn an_arrow_in_an_ordinary_file_name_is_not_mistaken_for_a_rename() {
        let entries = parse_status("?? weird -> name.txt\n M another -> odd.txt\n");
        assert_eq!(
            entries,
            vec![
                ChangedFile {
                    status: "??".to_owned(),
                    path: "weird -> name.txt".to_owned(),
                },
                ChangedFile {
                    status: " M".to_owned(),
                    path: "another -> odd.txt".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_rename_reports_the_new_path() {
        let entries = parse_status("R  old.txt -> new.txt\n");
        assert_eq!(
            entries,
            vec![ChangedFile {
                status: "R ".to_owned(),
                path: "new.txt".to_owned(),
            }]
        );
    }

    fn git(path: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }
}
