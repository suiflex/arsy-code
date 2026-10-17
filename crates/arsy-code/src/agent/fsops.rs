//! The workspace file operations, as contracts the registry can dispatch.
//!
//! One executor covers all of them because they differ only in which
//! [`Workspace`] call they make and which capability that call needs. Splitting
//! them into seven types would duplicate the artifact write, the resource
//! naming, and the error mapping seven times over for no case that varies.
//!
//! Every one of them goes through [`Workspace`], so confinement is decided in
//! one place; none of them touch `std::fs` directly.

use crate::{
    agent::instructions::Skill,
    edit::{self, EditAddress, EditOperation},
    operations::Directories,
    resource::{
        absolute, escapes, locate, open_outside, DirEntry, Location, ResolveError, Workspace,
    },
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The most one call will read or write. Larger than a source file and smaller
/// than anything a turn could carry, so the bound is hit by a mistake rather
/// than by ordinary work.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// What a file operation does. The kind string is the operation's identity, so
/// it is defined here rather than at each construction site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileOperation {
    Read,
    List,
    Write,
    Create,
    Edit,
    Delete,
    Move,
}

impl FileOperation {
    pub const ALL: [Self; 7] = [
        Self::Read,
        Self::List,
        Self::Write,
        Self::Create,
        Self::Edit,
        Self::Delete,
        Self::Move,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Read => "fs.read",
            Self::List => "fs.list",
            Self::Write => "fs.write",
            Self::Create => "fs.create",
            Self::Edit => "fs.edit",
            Self::Delete => "fs.delete",
            Self::Move => "fs.move",
        }
    }

    const fn action(self) -> CapabilityAction {
        match self {
            Self::Read | Self::List => CapabilityAction::FsRead,
            Self::Write | Self::Create | Self::Edit | Self::Move => CapabilityAction::FsWrite,
            Self::Delete => CapabilityAction::FsDelete,
        }
    }

    /// Reading twice is the same answer; editing twice is not the same file.
    const fn idempotency(self) -> Idempotency {
        match self {
            Self::Read | Self::List | Self::Write => Idempotency::Idempotent,
            Self::Create | Self::Edit | Self::Delete | Self::Move => Idempotency::Effectful,
        }
    }

    /// Whether the effect can be undone.
    ///
    /// Rewriting or moving a file leaves the previous content in version
    /// control; removing one does not, and neither does anything the harness
    /// keeps. Policy raises an irreversible call to approval however permissive
    /// a rule is, so this is the line between "an operator who allowed edits
    /// gets edits" and "an operator is asked about every line the model
    /// writes".
    const fn reversible(self) -> bool {
        !matches!(self, Self::Delete)
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let number = |name: &str| (name.to_owned(), JsonType::Number);
        let (required, optional) = match self {
            Self::Read => (
                vec![string("path")],
                vec![number("offset"), number("limit")],
            ),
            Self::List => (Vec::new(), vec![string("path")]),
            Self::Write | Self::Create => (vec![string("path"), string("content")], Vec::new()),
            Self::Edit => (
                vec![string("path"), string("old_text"), string("new_text")],
                vec![number("occurrence")],
            ),
            Self::Delete => (vec![string("path")], Vec::new()),
            Self::Move => (vec![string("from"), string("to")], Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

/// The bytes a read returned, plus what the model needs in order to ask a
/// better second question: whether it saw the whole file, and where it stopped.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadResult {
    pub path: String,
    pub text: String,
    /// One-based line the excerpt starts at, so a later edit can be addressed.
    pub first_line: u64,
    pub lines_returned: u64,
    pub total_lines: u64,
    pub truncated: bool,
    pub binary: bool,
    pub digest: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListResult {
    pub path: String,
    pub entries: Vec<ListEntry>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListEntry {
    pub name: String,
    pub directory: bool,
    pub bytes: u64,
}

/// What a mutation did. `digest` is the file's state afterwards, which is what
/// a caller checks before editing the same file again.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteResult {
    pub path: String,
    pub created: bool,
    pub bytes: u64,
    pub digest: String,
    /// The replaced text, when this result came from `fs.edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The replacement text, when this result came from `fs.edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// One-based line where the edit anchor started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_line: Option<u64>,
}

pub struct FileExecutor {
    operation: FileOperation,
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    /// The skills the prompt listed, so `skill://<name>` resolves to the file
    /// the listing pointed at. Empty for every operation but `fs.read`, and
    /// empty is fine there: a read of a path resolves as a path.
    skills: Vec<Skill>,
    /// Directories the operator added beside the workspace. Every operation
    /// works in them as it does in the workspace; the approval mode, not the
    /// executor, decides whether it may.
    directories: Directories,
}

impl FileExecutor {
    pub fn new(
        operation: FileOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
        skills: Vec<Skill>,
        directories: Directories,
    ) -> Arc<Self> {
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: operation.schema(),
                actions: vec![operation.action()],
                idempotency: operation.idempotency(),
                reversible: operation.reversible(),
                concurrency: match operation {
                    FileOperation::Read | FileOperation::List => ConcurrencyRule::Parallel,
                    _ => ConcurrencyRule::ExclusivePerResource,
                },
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
            skills,
            directories,
        })
    }

    fn open(&self) -> Result<Workspace, OperationError> {
        Workspace::open(&self.workspace)
            .map_err(|error| OperationError::Execution(error.to_string()))
    }

    /// The root a path lives under, opened, and the path relative to it.
    ///
    /// The workspace and every added directory are open to the operation; a
    /// path outside all of them runs only when the operator approved that
    /// exact path, which arrives as a grant naming it. A broad rule such as
    /// `file:**` is not that approval — the runtime asks before such a call,
    /// and this is the check that holds if a caller skipped asking.
    /// `directory` opens an approved outside path as a root of its own, for
    /// listing or searching it, rather than as a file under its parent.
    fn target(
        &self,
        path: &str,
        grants: &[CapabilityGrant],
        directory: bool,
    ) -> Result<(Workspace, PathBuf), OperationError> {
        let reading = self.operation.action() == CapabilityAction::FsRead;
        let roots = self.directories.roots(reading);
        match locate(&roots, &self.workspace, path).map_err(resolve)? {
            Location::Inside => Ok((self.open()?, PathBuf::from(path))),
            Location::Rooted(root, relative) => Ok((root, relative)),
            Location::Undeclared(absolute) => {
                if !approved(grants, self.operation.action(), path) {
                    return Err(resolve(ResolveError::OutsideWorkspace));
                }
                open_outside(&absolute, directory).map_err(resolve)
            }
        }
    }

    /// Read a file, or a skill the prompt listed by name.
    ///
    /// A home-declared skill is absolute and outside the workspace, and the
    /// listing already named it, so it is read where it lives; anything else,
    /// including an absolute path the model typed itself, goes through
    /// [`Self::target`].
    fn read(
        &self,
        requested: &str,
        grants: &[CapabilityGrant],
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<(ReadResult, String), OperationError> {
        let declared = skill_path(requested, &self.skills).transpose()?;
        let path = declared.clone().unwrap_or_else(|| requested.to_owned());
        if let Some(declared) = declared.filter(|declared| Path::new(declared).is_absolute()) {
            return Ok((read_declared(&declared, offset, limit)?, path));
        }
        let (root, relative) = self.target(&path, grants, false)?;
        let content = root.read(relative, MAX_FILE_BYTES).map_err(resolve)?;
        Ok((window(&path, content, offset, limit)?, path))
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
}

impl OperationExecutor for FileExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        let string = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| input.get(key).and_then(Value::as_u64);

        let (value, touched, state) = match self.operation {
            FileOperation::List => {
                let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
                let (root, relative) = self.target(path, grants, true)?;
                let entries = root.list(relative).map_err(resolve)?;
                (
                    self.put(
                        &ListResult {
                            path: path.to_owned(),
                            entries: entries.into_iter().map(entry).collect(),
                        },
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    None,
                )
            }
            FileOperation::Read => {
                let (result, path) =
                    self.read(string("path"), grants, number("offset"), number("limit"))?;
                let digest = result.digest.clone();
                (
                    self.put(&result, request.actor.clone())?,
                    path,
                    Some(digest),
                )
            }
            FileOperation::Write | FileOperation::Create => {
                let path = string("path");
                let content = string("content");
                let (root, relative) = self.target(path, grants, false)?;
                // Metadata, not a read: asking whether a file is there by
                // reading it costs the whole file before overwriting it.
                let existed = root.exists(&relative);
                let digest = if self.operation == FileOperation::Create {
                    root.create_new(&relative, content.as_bytes())
                } else {
                    root.write(&relative, content.as_bytes())
                }
                .map_err(resolve)?;
                (
                    self.put(
                        &WriteResult {
                            path: path.to_owned(),
                            created: !existed,
                            bytes: content.len() as u64,
                            digest: digest.to_string(),
                            before: (!existed).then(String::new),
                            after: (!existed).then(|| content.to_owned()),
                            first_line: (!existed).then_some(1),
                        },
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    Some(digest.to_string()),
                )
            }
            FileOperation::Edit => {
                let path = string("path");
                let (root, relative) = self.target(path, grants, false)?;
                let result = apply_edit(
                    &root,
                    &relative,
                    path,
                    string("old_text"),
                    string("new_text"),
                    number("occurrence"),
                )?;
                let digest = result.digest.clone();
                (
                    self.put(&result, request.actor.clone())?,
                    path.to_owned(),
                    Some(digest),
                )
            }
            FileOperation::Delete => {
                let path = string("path");
                let (root, relative) = self.target(path, grants, false)?;
                root.remove(relative).map_err(resolve)?;
                (
                    self.put(
                        &json!({"path": path, "deleted": true}),
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    None,
                )
            }
            FileOperation::Move => {
                let from = string("from");
                let to = string("to");
                // Each end is its own requirement, so policy and an approval
                // answer about both. They must still resolve to one root: a
                // rename cannot cross capability directories.
                let (root, source) = self.target(from, grants, false)?;
                let (other, destination) = self.target(to, grants, false)?;
                if root.path() != other.path() {
                    return Err(resolve(ResolveError::OutsideWorkspace));
                }
                root.rename(source, destination).map_err(resolve)?;
                (
                    self.put(&json!({"from": from, "to": to}), request.actor.clone())?,
                    to.to_owned(),
                    None,
                )
            }
        };

        // A path that leaves the workspace is recorded by where it actually
        // is, so the audit trail names the other directory rather than a
        // workspace path that does not exist.
        let (scheme, touched) = if escapes(Path::new(&touched)) {
            (
                "file",
                absolute(&self.workspace, &touched)
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            ("workspace", touched)
        };
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: self.operation.action(),
                resource: ResourceRef::new(scheme, touched)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: state.and_then(|digest| digest.parse().ok()),
        })
    }
}

fn entry(entry: DirEntry) -> ListEntry {
    ListEntry {
        name: entry.name,
        directory: entry.directory,
        bytes: entry.bytes,
    }
}

/// The scheme a skill is addressed by, matching the ecosystem family's
/// convention: the prompt lists skills by name and the model reads one with
/// `skill://<name>`.
const SKILL_SCHEME: &str = "skill://";

/// Resolve a `skill://<name>` path to the file it names, or `None` when the
/// path is not a skill reference.
///
/// Skills live in more than one ecosystem directory, so the name is matched
/// against what discovery found rather than against one fixed root. A name
/// nothing declared is an error rather than a miss: the model read the
/// listing before it asked, so a miss means the listing was stale and saying
/// so is more useful than an empty result.
fn skill_path(path: &str, skills: &[Skill]) -> Option<Result<String, OperationError>> {
    let name = path.strip_prefix(SKILL_SCHEME)?.trim_matches('/');
    if name.is_empty() {
        return Some(Err(OperationError::Execution(format!(
            "`{path}` names no skill"
        ))));
    }
    Some(
        skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.path.clone())
            .ok_or_else(|| {
                OperationError::Execution(format!(
                    "no skill named `{name}` is listed for this session"
                ))
            }),
    )
}

/// Read a skill the operator's own home declared.
///
/// Its `SKILL.md` is outside the workspace, so [`Workspace`] refuses the
/// absolute path — but the listing the model read already named that file, and
/// a skill it can see and cannot open is worse than no listing at all. Only a
/// path [`skill_path`] resolved reaches here.
fn read_declared(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<ReadResult, OperationError> {
    let content = crate::resource::read_declared(Path::new(path), MAX_FILE_BYTES)
        .map_err(|error| OperationError::Execution(error.to_string()))?;
    window(path, content, offset, limit)
}

/// The window of a file's content a read returns, shared by both readers so a
/// skill and a workspace file are reported the same way.
///
/// The window is in lines rather than bytes because that is the unit a model
/// asks in and the unit an error message reports in; a byte offset would let a
/// second read start mid-character.
fn window(
    path: &str,
    content: crate::resource::FileContent,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<ReadResult, OperationError> {
    let digest = content.digest.to_string();
    if content.is_binary() {
        // A binary file is reported rather than decoded: lossy UTF-8 would fill
        // a turn with replacement characters and teach the model nothing.
        return Ok(ReadResult {
            path: path.to_owned(),
            text: String::new(),
            first_line: 0,
            lines_returned: 0,
            total_lines: 0,
            truncated: false,
            binary: true,
            digest,
        });
    }
    let text = String::from_utf8(content.bytes)
        .map_err(|error| OperationError::Execution(error.to_string()))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as u64;
    let start = offset.unwrap_or(1).max(1) - 1;
    let count = limit.unwrap_or(u64::MAX);
    let window: Vec<&str> = lines
        .iter()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .take(usize::try_from(count).unwrap_or(usize::MAX))
        .copied()
        .collect();
    let returned = window.len() as u64;
    Ok(ReadResult {
        path: path.to_owned(),
        text: window.join("\n"),
        first_line: start + 1,
        lines_returned: returned,
        total_lines: total,
        truncated: start + returned < total,
        binary: false,
        digest,
    })
}

/// Replace one occurrence of `old_text`, refusing an ambiguous one.
///
/// Delegates to the edit engine so an agent edit and a transactional edit share
/// one matcher: an anchor that appears twice is [`edit::EditError::Ambiguous`]
/// here for the same reason it is there, rather than silently taking the first.
fn apply_edit(
    workspace: &Workspace,
    relative: &Path,
    path: &str,
    old_text: &str,
    new_text: &str,
    occurrence: Option<u64>,
) -> Result<WriteResult, OperationError> {
    if old_text.is_empty() {
        return Err(OperationError::Schema(
            "old_text must not be empty; use fs.write to replace a whole file".into(),
        ));
    }
    let occurrence = match occurrence {
        Some(value) => Some(
            u32::try_from(value)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| OperationError::Schema("occurrence is one-based".into()))?,
        ),
        None => None,
    };
    let before_bytes = workspace
        .read(relative, MAX_FILE_BYTES)
        .map_err(resolve)?
        .bytes;
    let before_text = String::from_utf8_lossy(&before_bytes);
    let first_line = line_number(&before_text, old_text, occurrence);
    let edits = edit::apply_unversioned(
        workspace.path(),
        &[EditOperation {
            path: relative.to_path_buf(),
            address: EditAddress::TextAnchor {
                needle: old_text.to_owned(),
                occurrence,
            },
            replacement: new_text.as_bytes().to_vec(),
        }],
    )
    .map_err(|error| OperationError::Execution(error.to_string()))?;
    let applied = edits
        .first()
        .ok_or_else(|| OperationError::Execution("edit applied nothing".into()))?;
    Ok(WriteResult {
        path: path.to_owned(),
        created: false,
        bytes: workspace
            .read(relative, MAX_FILE_BYTES)
            .map(|content| content.bytes.len() as u64)
            .unwrap_or_default(),
        digest: applied.after.to_string(),
        before: Some(old_text.to_owned()),
        after: Some(new_text.to_owned()),
        first_line: Some(first_line),
    })
}
/// Find the one-based line where the selected text anchor starts.
fn line_number(text: &str, needle: &str, occurrence: Option<NonZeroU32>) -> u64 {
    let wanted = occurrence.map_or(1, NonZeroU32::get) as usize;
    let position = text
        .match_indices(needle)
        .nth(wanted.saturating_sub(1))
        .map_or(0, |(position, _)| position);
    text[..position]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u64
        + 1
}

/// Whether the operator approved this exact path for this action.
fn approved(grants: &[CapabilityGrant], action: CapabilityAction, path: &str) -> bool {
    ResourceRef::new(action.default_scheme(), path).is_ok_and(|resource| {
        grants
            .iter()
            .any(|grant| grant.action == action && grant.names_exactly(&resource))
    })
}

/// A resolve failure the model can act on.
///
/// Path restrictions and existence conflicts are decisions, not I/O faults;
/// report them as refused arguments rather than a transient execution error.
fn resolve(error: ResolveError) -> OperationError {
    match error {
        ResolveError::OutsideWorkspace
        | ResolveError::EmptyPath
        | ResolveError::AlreadyExists
        | ResolveError::GitMetadata
        | ResolveError::Symlinked(_) => OperationError::Schema(error.to_string()),
        other => OperationError::Execution(other.to_string()),
    }
}

/// The registry entries this module contributes.
pub fn executors(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    skills: &[Skill],
    directories: &Directories,
) -> Vec<Arc<dyn OperationExecutor>> {
    FileOperation::ALL
        .into_iter()
        .map(|operation| {
            FileExecutor::new(
                operation,
                workspace,
                Arc::clone(artifacts),
                retain_until_ms,
                skills.to_vec(),
                directories.clone(),
            ) as Arc<dyn OperationExecutor>
        })
        .collect()
}
