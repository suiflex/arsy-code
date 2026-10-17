use arsy_kernel::domain::{AgentId, ArtifactId, AttemptId, SessionId, TaskId};
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};

const MAX_SNAPSHOT_FILES: usize = 100_000;
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Git's own administration of a repository is not concurrent: `worktree
/// add`, `worktree prune`, and `merge` all take the index and the ref store,
/// and two of them at once produce a lock error rather than a queue. Work
/// *inside* different views is untouched by this — only the bookkeeping that
/// reaches the shared repository is serialized.
fn admin() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What to do when the source has uncommitted work.
///
/// There is no "ignore it" arm on purpose: a view cut from a dirty tree that
/// silently dropped the operator's changes produces a diff against a base
/// nobody can reconstruct.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirtyPolicy {
    /// Refuse to cut a view. The safe default: the operator decides what
    /// happens to their own uncommitted work.
    #[default]
    Refuse,
    /// Carry the uncommitted changes into the view as a patch, and say so.
    CarryPatch,
}

/// Who a view belongs to, as the parts a name can be built from without two
/// agents ever colliding.
#[derive(Clone, Copy, Debug)]
pub struct ViewOwner {
    pub session: SessionId,
    pub task: TaskId,
    pub attempt: AttemptId,
    pub agent: AgentId,
}

impl ViewOwner {
    /// A directory and ref name no other claim can produce.
    ///
    /// Every part is included because each one alone repeats: a task retried
    /// has two attempts, an agent may hold views in two sessions, and a name
    /// that collided would hand a retry the tree its predecessor left.
    pub fn slug(&self) -> String {
        let short = |id: &str| id.split('-').next().unwrap_or(id).to_owned();
        format!(
            "{}-{}-{}-{}",
            short(&self.session.to_string()),
            short(&self.task.to_string()),
            short(&self.attempt.to_string()),
            short(&self.agent.to_string()),
        )
    }

    /// The internal branch a writer commits on. Namespaced so it cannot be
    /// mistaken for anything a person made.
    pub fn branch(&self) -> String {
        format!("arsy/writer/{}", self.slug())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationBackend {
    GitWorktree,
    CopiedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLease {
    pub owner: AgentId,
    pub source: PathBuf,
    pub view: PathBuf,
    pub base_revision: String,
    pub backend: IsolationBackend,
    pub mutable: bool,
    pub expires_at_ms: u64,
    /// The internal branch a writer's commits land on, when the view is a
    /// Git worktree. `None` for readers and for copied snapshots.
    pub branch: Option<String>,
    /// Uncommitted work carried in from the source, when policy allowed it.
    pub carried_patch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeOutcome {
    Applied {
        commit: String,
    },
    Conflict {
        paths: Vec<PathBuf>,
        evidence: String,
    },
}

/// One writer's work, offered to the target.
///
/// A struct rather than arguments because the preflight reads most of it, and
/// a positional call with this many parts is how a caller ends up passing the
/// writer's base where the target's revision belongs.
pub struct IntegrationRequest<'a> {
    pub target: &'a Path,
    pub writer_revision: &'a str,
    /// What the writer started from, checked against where the target is now.
    pub base_revision: &'a str,
    pub changed_files: &'a [String],
    /// Paths already applied by an earlier integration in this round.
    pub already_integrated: &'a [String],
    /// Artifact ids of checks already verified by the caller against its
    /// session's validation log and writer attempt. This low-level coordinator
    /// cannot infer a session or attempt from a Git revision.
    pub validation: &'a [String],
    /// What the writer says it did not settle.
    pub unresolved: &'a [String],
    /// Where the target is expected to be, when the caller has a revision it
    /// decided against.
    pub expect_target_revision: Option<&'a str>,
    pub allow_stale_base: bool,
    pub policy_authorized: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recovery {
    ReuseCommitted { owner: AgentId, revision: String },
    ReclaimUncommitted { owner: AgentId },
}

/// Git commands that inspect an agent-controlled worktree must never execute
/// repository-configured fsmonitor or hook programs in the host.
fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            if cfg!(windows) { "NUL" } else { "/dev/null" }
        ));
    command
}

#[derive(Default)]
pub struct WorkspaceCoordinator {
    leases: BTreeMap<AgentId, WorkspaceLease>,
    readers: BTreeMap<(PathBuf, String), PathBuf>,
}

impl WorkspaceCoordinator {
    pub fn writer(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: AgentId,
        expires_at_ms: u64,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        self.writer_for(
            source,
            isolation_root,
            &ViewOwner {
                session: SessionId::new(),
                task: TaskId::new(),
                attempt: AttemptId::new(),
                agent: owner,
            },
            expires_at_ms,
            DirtyPolicy::default(),
        )
    }

    /// Cut a mutable view for one attempt, on its own branch.
    ///
    /// The branch matters as much as the directory: a detached worktree loses
    /// what the writer committed as soon as the view is pruned, and an
    /// integrator reading a dangling object is reading something Git is free
    /// to collect.
    pub fn writer_for(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: &ViewOwner,
        expires_at_ms: u64,
        dirty: DirtyPolicy,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        if self.leases.contains_key(&owner.agent) {
            return Err(WorkspaceError::OwnerAlreadyHasWriter(owner.agent));
        }
        let source = fs::canonicalize(source)?;
        fs::create_dir_all(isolation_root)?;
        let view = isolation_root.join(owner.slug());
        let _admin = admin();
        let (base_revision, backend, carried) = match git_revision(&source) {
            Ok(revision) => {
                let patch = match dirty {
                    DirtyPolicy::Refuse if has_uncommitted(&source)? => {
                        return Err(WorkspaceError::DirtySource(source));
                    }
                    DirtyPolicy::Refuse => None,
                    DirtyPolicy::CarryPatch => uncommitted(&source)?,
                };
                git(
                    &source,
                    [
                        "worktree",
                        "add",
                        "-b",
                        &owner.branch(),
                        path_text(&view)?,
                        &revision,
                    ],
                )?;
                if let Some(patch) = &patch {
                    apply_patch(&view, patch)?;
                }
                (revision, IsolationBackend::GitWorktree, patch)
            }
            Err(WorkspaceError::NotGit) => {
                copy_snapshot(&source, &view)?;
                (
                    crate::edit::workspace_version(&source)?.0.to_string(),
                    IsolationBackend::CopiedSnapshot,
                    None,
                )
            }
            Err(error) => return Err(error),
        };
        let lease = WorkspaceLease {
            owner: owner.agent,
            source,
            view,
            base_revision,
            backend,
            mutable: true,
            expires_at_ms,
            branch: Some(owner.branch()),
            carried_patch: carried,
        };
        write_lease_marker(&lease.view, expires_at_ms);
        self.leases.insert(owner.agent, lease.clone());
        Ok(lease)
    }

    pub fn reader(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: AgentId,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        let source = fs::canonicalize(source)?;
        let base = git_revision(&source).or_else(|error| match error {
            WorkspaceError::NotGit => Ok(crate::edit::workspace_version(&source)?.0.to_string()),
            other => Err(other),
        })?;
        let key = (source.clone(), base.clone());
        // A view someone else's cleanup removed is cut again rather than
        // handed out as a path that no longer exists.
        let view = if let Some(existing) = self.readers.get(&key).filter(|view| view.exists()) {
            existing.clone()
        } else {
            fs::create_dir_all(isolation_root)?;
            let view = isolation_root.join(format!("read-{base}"));
            let _admin = admin();
            if git_revision(&source).is_ok() {
                git(
                    &source,
                    ["worktree", "add", "--detach", path_text(&view)?, &base],
                )?;
            } else {
                copy_snapshot(&source, &view)?;
            }
            make_read_only(&view)?;
            self.readers.insert(key, view.clone());
            view
        };
        // A reader view is shared and never expires in this process, so the
        // marker another process reads is renewed on every use instead: a view
        // nobody asked for in a lease's length is one nobody is reading.
        write_lease_marker(&view, now_ms() + READER_LEASE_MS);
        let backend = if git_revision(&source).is_ok() {
            IsolationBackend::GitWorktree
        } else {
            IsolationBackend::CopiedSnapshot
        };
        Ok(WorkspaceLease {
            owner,
            source,
            view,
            base_revision: base,
            backend,
            mutable: false,
            expires_at_ms: u64::MAX,
            branch: None,
            carried_patch: None,
        })
    }

    pub fn recover_expired(&mut self, now_ms: u64) -> Result<Vec<Recovery>, WorkspaceError> {
        let owners: Vec<_> = self
            .leases
            .iter()
            .filter_map(|(owner, lease)| (lease.expires_at_ms <= now_ms).then_some(*owner))
            .collect();
        let mut output = Vec::new();
        for owner in owners {
            let lease = self.leases.remove(&owner).expect("selected above");
            let revision = git_revision(&lease.view).ok();
            if let Some(revision) = revision.filter(|revision| revision != &lease.base_revision) {
                output.push(Recovery::ReuseCommitted { owner, revision });
            } else {
                output.push(Recovery::ReclaimUncommitted { owner });
            }
        }
        Ok(output)
    }

    /// Apply one writer's work to the target, or say why it was not applied.
    ///
    /// Everything refusable is refused *before* the merge runs, because a
    /// merge that fails partway is a target left in conflict — and the
    /// invariant is that a refused integration leaves the target exactly as
    /// it was. The preflight is therefore not an optimisation: it is what
    /// makes "the target is unchanged" true.
    pub fn integrate(
        &self,
        request: &IntegrationRequest<'_>,
    ) -> Result<MergeOutcome, WorkspaceError> {
        if !request.policy_authorized {
            return Err(WorkspaceError::PolicyRequired);
        }
        if request.validation.is_empty()
            || request
                .validation
                .iter()
                .any(|id| id.parse::<ArtifactId>().is_err())
        {
            return Err(WorkspaceError::Unvalidated);
        }
        if !request.unresolved.is_empty() {
            return Err(WorkspaceError::Unresolved(request.unresolved.join(", ")));
        }
        let _admin = admin();
        let target_revision = git_revision(request.target)?;
        if let Some(expected) = request.expect_target_revision {
            if expected != target_revision {
                return Err(WorkspaceError::TargetMoved {
                    expected: expected.to_owned(),
                    actual: target_revision,
                });
            }
        }
        if has_uncommitted(request.target)? {
            return Err(WorkspaceError::DirtySource(request.target.to_owned()));
        }
        // A writer that started from a revision the target has since left is
        // proposing a change nobody validated against what is there now.
        if !request.allow_stale_base
            && !is_ancestor(request.target, request.base_revision, &target_revision)?
        {
            return Err(WorkspaceError::StaleBase {
                base: request.base_revision.to_owned(),
                target: target_revision,
            });
        }
        if let Some(path) = request
            .changed_files
            .iter()
            .find(|path| request.already_integrated.contains(&path.to_string()))
        {
            return Err(WorkspaceError::PathOverlap(path.to_string()));
        }
        self.merge_git(request.target, request.writer_revision, true)
    }

    /// Delete a view the coordinator cut, and forget the lease.
    ///
    /// Takes the branch with it only when it holds nothing the writer
    /// committed: a branch with work on it is evidence, and quarantining it
    /// is cheaper than explaining where it went.
    pub fn discard(&mut self, lease: &WorkspaceLease) -> Result<(), WorkspaceError> {
        let _admin = admin();
        self.leases.remove(&lease.owner);
        let _ = fs::remove_file(lease_marker(&lease.view));
        if lease.backend == IsolationBackend::GitWorktree {
            let _ = git(
                &lease.source,
                ["worktree", "remove", "--force", path_text(&lease.view)?],
            );
            let _ = git(&lease.source, ["worktree", "prune"]);
            if let Some(branch) = &lease.branch {
                // `-d` rather than `-D`: it deletes a branch that added
                // nothing and refuses one that did.
                let _ = git(&lease.source, ["branch", "-d", branch]);
            }
        } else if lease.view.exists() {
            fs::remove_dir_all(&lease.view)?;
        }
        Ok(())
    }

    /// Commit everything a writer left in its view, on its own branch.
    ///
    /// The harness does this rather than the writer, because committing is
    /// bookkeeping rather than engineering: a writer that had to run `git`
    /// would need process authority it was never given, and a writer that
    /// forgot would leave work an integrator cannot name a revision for.
    ///
    /// `None` when the view is unchanged — nothing to integrate is a fact,
    /// not a failure.
    pub fn commit_view(
        lease: &WorkspaceLease,
        message: &str,
    ) -> Result<Option<String>, WorkspaceError> {
        if lease.backend != IsolationBackend::GitWorktree {
            return Ok(None);
        }
        let _admin = admin();
        if uncommitted(&lease.view)?.is_none() {
            let head = git_revision(&lease.view)?;
            return Ok((head != lease.base_revision).then_some(head));
        }
        git(
            &lease.view,
            ["add", "--all", "--", ".", EXCLUDE_HARNESS_STATE],
        )?;
        git(
            &lease.view,
            [
                "-c",
                "user.name=ARSY",
                "-c",
                "user.email=arsy@localhost",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ],
        )?;
        Ok(Some(git_revision(&lease.view)?))
    }

    /// What one Git repository is, wherever it is checked out.
    ///
    /// The root commit, because a path is where a clone happens to live and a
    /// remote is a thing an operator can rename.
    pub fn repository_identity(source: &Path) -> Result<String, WorkspaceError> {
        Ok(git_output(source, ["rev-list", "--max-parents=0", "HEAD"])?
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    fn merge_git(
        &self,
        target: &Path,
        writer_revision: &str,
        policy_authorized: bool,
    ) -> Result<MergeOutcome, WorkspaceError> {
        if !policy_authorized {
            return Err(WorkspaceError::PolicyRequired);
        }
        if !matches!(writer_revision.len(), 40 | 64)
            || !writer_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(WorkspaceError::InvalidRevision);
        }
        let output = git_command()
            .args([
                "-C",
                path_text(target)?,
                "merge",
                "--no-edit",
                writer_revision,
            ])
            .output()?;
        if output.status.success() {
            return Ok(MergeOutcome::Applied {
                commit: git_revision(target)?,
            });
        }
        let conflicts = git_output(target, ["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(PathBuf::from)
            .collect();
        let evidence = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = git(target, ["merge", "--abort"]);
        Ok(MergeOutcome::Conflict {
            paths: conflicts,
            evidence,
        })
    }
}

fn git_revision(root: &Path) -> Result<String, WorkspaceError> {
    let output = git_command()
        .args(["-C", path_text(root)?, "rev-parse", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::NotGit);
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|error| WorkspaceError::Git(error.to_string()))?
        .trim()
        .to_owned())
}

fn git<const N: usize>(root: &Path, args: [&str; N]) -> Result<(), WorkspaceError> {
    let output = git_command()
        .arg("-C")
        .arg(plain(root))
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, WorkspaceError> {
    let output = git_command()
        .arg("-C")
        .arg(plain(root))
        .args(args)
        .output()?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|error| WorkspaceError::Git(error.to_string()))
    } else {
        Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// A refusal must inspect the tree without staging even an intent-to-add.
fn has_uncommitted(root: &Path) -> Result<bool, WorkspaceError> {
    let untracked = git_output(
        root,
        [
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            EXCLUDE_HARNESS_STATE,
        ],
    )?;
    if !untracked.is_empty() {
        return Ok(true);
    }
    Ok(!git_output(
        root,
        ["diff", "HEAD", "--binary", "--", ".", EXCLUDE_HARNESS_STATE],
    )?
    .is_empty())
}

/// The source's uncommitted work as a patch, or `None` when it is clean.
///
/// Both halves: `diff HEAD` covers tracked changes, and untracked files are
/// included by intent-to-add so a new file is not silently left behind.
fn uncommitted(root: &Path) -> Result<Option<String>, WorkspaceError> {
    let untracked = git_output(
        root,
        [
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            EXCLUDE_HARNESS_STATE,
        ],
    )?;
    for path in untracked.lines().filter(|line| !line.trim().is_empty()) {
        git(root, ["add", "--intent-to-add", "--", path])?;
    }
    let patch = git_output(
        root,
        ["diff", "HEAD", "--binary", "--", ".", EXCLUDE_HARNESS_STATE],
    )?;
    Ok((!patch.trim().is_empty()).then_some(patch))
}

/// ARSY's own state lives in the workspace, so every run makes the tree
/// "dirty" by writing its event store, its artifacts, and the very views this
/// module cuts. That is the harness talking about itself, not the operator's
/// uncommitted work, and treating it as the latter would refuse every writer
/// in a repository ARSY has ever run in.
const EXCLUDE_HARNESS_STATE: &str = ":(exclude).arsy/state/**";

/// The same directory, as a path prefix.
pub const HARNESS_STATE: &str = ".arsy";

/// Where everything ARSY writes on its own lives, apart from the files an
/// operator edits: deleting it loses history, never configuration.
pub const RUNTIME_STATE: &str = ".arsy/state";
/// The session event store.
pub const SESSION_STORE: &str = ".arsy/state/sessions.sqlite3";
/// Content-addressed artifacts: evidence, tool output, completion proofs.
pub const ARTIFACTS: &str = ".arsy/state/artifacts";
/// The git worktrees subagents run in.
pub const VIEWS: &str = ".arsy/state/views";
/// The trees `arsy eval` measures in.
pub const EVAL: &str = ".arsy/state/eval";
/// The repository map a turn is given as context.
pub const REPO_MAP: &str = ".arsy/state/repo-map.json";

/// Create the runtime directory, and when `ignore_itself` is set, the
/// `.gitignore` that keeps it out of the repository without the operator
/// adding anything. An existing `.gitignore` is left as it is.
pub fn ensure_state_dir(root: &Path, ignore_itself: bool) -> io::Result<PathBuf> {
    let state = root.join(RUNTIME_STATE);
    fs::create_dir_all(&state)?;
    let ignore = state.join(".gitignore");
    if ignore_itself && !ignore.exists() {
        fs::write(&ignore, "*\n")?;
    }
    Ok(state)
}

/// How long a reader view's marker says it is in use after its last lease.
const READER_LEASE_MS: u64 = 30 * 60 * 1_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The file beside a view that says until when it is leased.
///
/// Leases live in the memory of the process that cut them, so another
/// process — `arsy storage clean views` in a second terminal — cannot see
/// them. This marker is what it reads instead of guessing from timestamps.
pub fn lease_marker(view: &Path) -> PathBuf {
    let mut name = view.as_os_str().to_owned();
    name.push(".lease");
    PathBuf::from(name)
}

fn write_lease_marker(view: &Path, expires_at_ms: u64) {
    // Best effort: a missing marker falls back to the older, cruder rule,
    // which is what every view cut before markers existed gets anyway.
    let _ = fs::write(lease_marker(view), expires_at_ms.to_string());
}

/// Whether the view's lease has run out: `None` when it has no marker.
pub fn lease_expired(view: &Path, now_ms: u64) -> Option<bool> {
    let text = fs::read_to_string(lease_marker(view)).ok()?;
    let expires_at_ms: u64 = text.trim().parse().ok()?;
    Some(expires_at_ms <= now_ms)
}

/// Where each piece of runtime state lived before `.arsy/state/`.
const LEGACY_STATE: [(&str, &str); 2] = [
    (".arsy/repo-map.json", REPO_MAP),
    (".arsy/artifacts", ARTIFACTS),
];
/// The old session store; its WAL sidecars move with it.
const LEGACY_SESSION_STORE: &str = ".arsy/sessions.sqlite3";

/// Move runtime state an older ARSY left directly under `.arsy/` into
/// `.arsy/state/`, once.
///
/// A rename, never a copy, and never over something already there: a
/// destination that exists means the move happened or a newer ARSY started
/// fresh, and either way the file at the destination is the one in use. The
/// session store moves together with its `-wal` and `-shm` sidecars, which
/// hold committed transactions not yet folded into the main file, so the log
/// is checkpointed into the database first and a store another process is
/// writing to is left for a later run; if any of the three cannot move, the
/// ones that did are put back.
///
/// `views/` and `eval/` are git worktrees and stay where they are: renaming
/// one would leave git pointing at a directory that no longer exists.
///
/// Failures are silent, as the user configuration bootstrap's are: a
/// workspace whose old state cannot move runs with fresh state rather than
/// not at all.
pub fn migrate_state(root: &Path) {
    let has_legacy = LEGACY_STATE.iter().any(|(old, _)| root.join(old).exists())
        || root.join(LEGACY_SESSION_STORE).exists();
    // The directory only: whether it ignores itself is configuration, which
    // the store decides when it opens, a moment later.
    if !has_legacy || ensure_state_dir(root, false).is_err() {
        return;
    }
    for (old, new) in LEGACY_STATE {
        let (old, new) = (root.join(old), root.join(new));
        if old.exists() && !new.exists() {
            let _ = fs::rename(&old, &new);
        }
    }
    let _ = move_session_store(&root.join(LEGACY_SESSION_STORE), &root.join(SESSION_STORE));
}

/// Move an SQLite database and its WAL sidecars as one.
fn move_session_store(old: &Path, new: &Path) -> io::Result<()> {
    if !old.exists() || new.exists() {
        return Ok(());
    }
    // A store another process is writing to stays where it is until a later
    // run; so does one whose log will not fold in. Only a store whose file
    // alone holds every commit is moved, so no step can strand a commit in a
    // log separated from its database.
    let busy = arsy_kernel::sqlite::is_being_written(old)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let folded = arsy_kernel::sqlite::checkpoint(old)
        .map_err(|error| io::Error::other(error.to_string()))?;
    if busy || !folded {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the session store is in use; it moves on a later run",
        ));
    }
    let sidecar = |path: &Path, suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    // The database first: after the checkpoint its sidecars hold nothing a
    // reader needs, so a stop between steps leaves a complete database at the
    // destination and at worst an empty log behind.
    let pairs: Vec<(PathBuf, PathBuf)> = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| (sidecar(old, suffix), sidecar(new, suffix)))
        .filter(|(from, _)| from.exists())
        .collect();
    let mut moved = Vec::new();
    for (from, to) in &pairs {
        if to.exists() {
            restore(&moved);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
        if let Err(error) = fs::rename(from, to) {
            restore(&moved);
            return Err(error);
        }
        moved.push((from.clone(), to.clone()));
    }
    Ok(())
}

fn restore(moved: &[(PathBuf, PathBuf)]) {
    for (from, to) in moved.iter().rev() {
        let _ = fs::rename(to, from);
    }
}

fn apply_patch(view: &Path, patch: &str) -> Result<(), WorkspaceError> {
    let file = view.join(".arsy-carried.patch");
    fs::write(&file, patch)?;
    let applied = git(view, ["apply", "--allow-empty", path_text(&file)?]);
    fs::remove_file(&file)?;
    applied
}

/// Whether `ancestor` is in `descendant`'s history.
fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, WorkspaceError> {
    Ok(git_command()
        .args([
            "-C",
            path_text(root)?,
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ])
        .output()?
        .status
        .success())
}

fn copy_snapshot(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    // Made first rather than on the first entry copied: a workspace with
    // nothing in it is still a workspace, and a view that does not exist is
    // not one anybody can open.
    fs::create_dir_all(destination)?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in walker(source) {
        let entry = entry.map_err(walk_error)?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| WorkspaceError::InvalidPath(entry.path().to_owned()))?;
        // ARSY's own state, including the directory these views are cut
        // into. Copying it would copy the snapshot into itself.
        if relative.as_os_str().is_empty() || relative.starts_with(HARNESS_STATE) {
            continue;
        }
        let target = destination.join(relative);
        let kind = entry
            .file_type()
            .ok_or_else(|| WorkspaceError::UnsupportedFile(entry.path().to_owned()))?;
        if kind.is_dir() {
            fs::create_dir_all(target)?;
        } else if kind.is_file() {
            files += 1;
            bytes = bytes
                .checked_add(entry.metadata().map_err(walk_error)?.len())
                .ok_or(WorkspaceError::SnapshotTooLarge)?;
            if files > MAX_SNAPSHOT_FILES || bytes > MAX_SNAPSHOT_BYTES {
                return Err(WorkspaceError::SnapshotTooLarge);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), target)?;
        } else {
            return Err(WorkspaceError::UnsupportedFile(entry.path().to_owned()));
        }
    }
    Ok(())
}

fn make_read_only(root: &Path) -> Result<(), WorkspaceError> {
    for entry in walker(root) {
        let entry = entry.map_err(walk_error)?;
        let metadata = entry.metadata().map_err(walk_error)?;
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(entry.path(), permissions)?;
    }
    Ok(())
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .build()
}

fn walk_error(error: ignore::Error) -> WorkspaceError {
    WorkspaceError::Io(io::Error::other(error))
}

fn path_text(path: &Path) -> Result<&str, WorkspaceError> {
    plain(path)
        .to_str()
        .ok_or_else(|| WorkspaceError::InvalidPath(path.to_owned()))
}

/// A path Git will take.
///
/// `fs::canonicalize` on Windows returns an extended-length path — `\\?\C:\…`
/// — and Git refuses one as an argument: `could not create leading
/// directories of '//?/C:/…': Invalid argument`. Every path this module hands
/// Git was canonicalised here from a directory that exists, so the prefix is
/// carrying nothing but the length escape, and dropping it loses nothing.
///
/// Only a drive path. `\\?\UNC\server\share` means something else, and the
/// remainder after the prefix is not a path on its own.
fn plain(path: &Path) -> &Path {
    let Some(text) = path.to_str() else {
        return path;
    };
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return path;
    };
    let drive = rest.as_bytes();
    if drive.len() >= 2 && drive[0].is_ascii_alphabetic() && drive[1] == b':' {
        return Path::new(rest);
    }
    path
}

#[derive(Debug)]
pub enum WorkspaceError {
    OwnerAlreadyHasWriter(AgentId),
    PolicyRequired,
    NotGit,
    SnapshotTooLarge,
    UnsupportedFile(PathBuf),
    InvalidPath(PathBuf),
    InvalidRevision,
    /// Uncommitted work where the caller needs a known tree.
    DirtySource(PathBuf),
    /// The writer started from a revision the target has since left.
    StaleBase {
        base: String,
        target: String,
    },
    /// The target is not where the caller decided it was.
    TargetMoved {
        expected: String,
        actual: String,
    },
    /// A path an earlier integration in this round already applied.
    PathOverlap(String),
    /// No recorded check backs the change.
    Unvalidated,
    /// The writer says it left something unsettled.
    Unresolved(String),
    Git(String),
    Edit(crate::edit::EditError),
    Io(io::Error),
}

impl WorkspaceError {
    /// The whole message, for a variant that has nothing to interpolate.
    const fn constant(&self) -> Option<&'static str> {
        Some(match self {
            Self::PolicyRequired => "workspace merge requires policy authorization",
            Self::NotGit => "workspace is not a Git repository",
            Self::SnapshotTooLarge => "workspace snapshot exceeds bounded limits",
            Self::InvalidRevision => "writer revision must be a full hexadecimal object ID",
            Self::Unvalidated => "integration requires recorded validation evidence",
            _ => return None,
        })
    }

    /// Something about a path this module was given.
    fn describe_path(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        Some(match self {
            Self::UnsupportedFile(path) => write!(
                formatter,
                "snapshot refuses special file {}",
                path.display()
            ),
            Self::InvalidPath(path) => write!(formatter, "path is not UTF-8: {}", path.display()),
            Self::DirtySource(path) => write!(
                formatter,
                "{} has uncommitted work; commit, stash, or allow it to be carried",
                path.display()
            ),
            Self::PathOverlap(path) => {
                write!(formatter, "{path} was already changed by this round")
            }
            _ => return None,
        })
    }

    /// A reason an integration was refused before it ran.
    fn describe_refusal(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        Some(match self {
            Self::OwnerAlreadyHasWriter(owner) => {
                write!(formatter, "agent {owner} already owns a mutable workspace")
            }
            Self::StaleBase { base, target } => write!(
                formatter,
                "the writer started from {base}, which is not in {target}'s history"
            ),
            Self::TargetMoved { expected, actual } => write!(
                formatter,
                "the target was {expected} when this was decided and is {actual} now"
            ),
            Self::Unresolved(what) => write!(formatter, "the writer left {what} unresolved"),
            _ => return None,
        })
    }
}

impl fmt::Display for WorkspaceError {
    /// The constants, then two tables by subject, then what came from
    /// somewhere else: Git's own words, and the errors this wraps.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(message) = self.constant() {
            return formatter.write_str(message);
        }
        if let Some(written) = self
            .describe_path(formatter)
            .or_else(|| self.describe_refusal(formatter))
        {
            return written;
        }
        match self {
            Self::Git(message) => formatter.write_str(message),
            Self::Edit(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            // Unreachable: every other variant is claimed above, and a new
            // one that is not stops compiling in whichever table it belongs
            // to rather than reaching here silently.
            other => write!(formatter, "workspace error: {other:?}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::edit::EditError> for WorkspaceError {
    fn from(value: crate::edit::EditError) -> Self {
        Self::Edit(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real SQLite store in WAL mode with one committed row, the way ARSY
    /// left it: a store the migration can checkpoint and has to keep whole.
    fn legacy_store(path: &Path) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('kept');")
            .unwrap();
    }

    /// What the store at `path` holds, read with no sidecar beside it.
    fn stored(path: &Path) -> String {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT v FROM t", [], |row| row.get(0))
            .unwrap()
    }

    fn legacy_workspace() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let arsy = root.path().join(".arsy");
        fs::create_dir_all(arsy.join("artifacts/ab")).unwrap();
        fs::write(arsy.join("artifacts/ab/blob"), "evidence").unwrap();
        fs::write(arsy.join("repo-map.json"), "{}").unwrap();
        legacy_store(&arsy.join("sessions.sqlite3"));
        fs::create_dir_all(arsy.join("views/agent")).unwrap();
        fs::create_dir_all(arsy.join("eval/trial")).unwrap();
        fs::write(arsy.join("arsy.json"), "{}").unwrap();
        root
    }

    #[test]
    fn migrate_moves_old_layout_into_state() {
        let root = legacy_workspace();
        migrate_state(root.path());
        // forgeguard: allow FG-SEC-007 -- test helper reading a file the test itself wrote in a tempdir
        let read = |path: &str| fs::read_to_string(root.path().join(path)).unwrap();
        assert_eq!(read(".arsy/state/artifacts/ab/blob"), "evidence");
        assert_eq!(read(REPO_MAP), "{}");
        assert_eq!(stored(&root.path().join(SESSION_STORE)), "kept");
        for gone in [
            "artifacts",
            "repo-map.json",
            "sessions.sqlite3",
            "sessions.sqlite3-wal",
        ] {
            assert!(
                !root.path().join(".arsy").join(gone).exists(),
                "{gone} moved"
            );
        }
        assert_eq!(read(".arsy/arsy.json"), "{}", "configuration stays");
        // Whether it ignores itself is configuration, which the store applies
        // when it opens; the move only makes the directory.
        assert!(!root.path().join(".arsy/state/.gitignore").exists());
    }

    #[test]
    fn migrate_leaves_views_and_eval() {
        let root = legacy_workspace();
        migrate_state(root.path());
        assert!(root.path().join(".arsy/views/agent").is_dir());
        assert!(root.path().join(".arsy/eval/trial").is_dir());
        assert!(!root.path().join(VIEWS).exists());
    }

    #[test]
    fn migrate_never_overwrites_existing_state() {
        let root = legacy_workspace();
        fs::create_dir_all(root.path().join(RUNTIME_STATE)).unwrap();
        fs::write(root.path().join(SESSION_STORE), "new db").unwrap();
        fs::write(root.path().join(REPO_MAP), "new map").unwrap();

        migrate_state(root.path());

        // forgeguard: allow FG-SEC-007 -- test helper reading a file the test itself wrote in a tempdir
        let read = |path: &str| fs::read_to_string(root.path().join(path)).unwrap();
        assert_eq!(read(SESSION_STORE), "new db");
        assert_eq!(read(REPO_MAP), "new map");
        assert_eq!(
            stored(&root.path().join(".arsy/sessions.sqlite3")),
            "kept",
            "the old store stays put, whole"
        );
    }

    #[test]
    fn migrate_moves_sqlite_sidecars_together_or_not_at_all() {
        // A connection left open keeps the log and index files on disk, so
        // all three have to move; a stale index at the destination stops the
        // move after the database and its log went across, and both come back.
        let root = legacy_workspace();
        let old = root.path().join(".arsy/sessions.sqlite3");
        let new = root.path().join(".arsy/state/sessions.sqlite3");
        let open = rusqlite::Connection::open(&old).unwrap();
        let _: i64 = open
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        fs::create_dir_all(root.path().join(RUNTIME_STATE)).unwrap();
        fs::write(
            root.path().join(".arsy/state/sessions.sqlite3-shm"),
            "stale",
        )
        .unwrap();

        assert!(move_session_store(&old, &new).is_err());

        assert_eq!(stored(&old), "kept", "the database came back");
        assert!(
            root.path().join(".arsy/sessions.sqlite3-wal").exists(),
            "with its log"
        );
        assert!(!new.exists());
        assert!(!root
            .path()
            .join(".arsy/state/sessions.sqlite3-wal")
            .exists());
    }

    #[test]
    fn a_store_being_written_is_left_for_a_later_run() {
        let root = legacy_workspace();
        let old = root.path().join(".arsy/sessions.sqlite3");
        let writer = rusqlite::Connection::open(&old).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();

        migrate_state(root.path());

        assert!(old.exists(), "nothing moved under a writer");
        assert!(!root.path().join(SESSION_STORE).exists());
        writer.execute_batch("ROLLBACK;").unwrap();
        drop(writer);

        migrate_state(root.path());
        assert!(!old.exists());
        assert_eq!(stored(&root.path().join(SESSION_STORE)), "kept");
    }

    #[test]
    fn a_workspace_without_old_state_gets_no_state_directory() {
        let root = tempfile::tempdir().unwrap();
        migrate_state(root.path());
        assert!(!root.path().join(".arsy").exists());
    }

    #[test]
    fn the_state_directory_ignores_itself() {
        let root = tempfile::tempdir().unwrap();
        let status = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        status(&["init", "-q"]);
        let state = ensure_state_dir(root.path(), true).unwrap();
        fs::write(state.join("sessions.sqlite3"), "db").unwrap();
        assert_eq!(
            status(&["status", "--porcelain", "--untracked-files=all"]),
            ""
        );

        let quiet = tempfile::tempdir().unwrap();
        let state = ensure_state_dir(quiet.path(), false).unwrap();
        assert!(!state.join(".gitignore").exists());
    }

    /// Everything ARSY writes on its own sits under one directory, so an
    /// operator can delete or ignore it without touching configuration.
    #[test]
    fn every_runtime_path_is_under_the_runtime_state() {
        for path in [SESSION_STORE, ARTIFACTS, VIEWS, EVAL, REPO_MAP] {
            assert!(
                Path::new(path).starts_with(RUNTIME_STATE),
                "{path} is outside {RUNTIME_STATE}"
            );
        }
    }

    #[test]
    fn copied_writers_are_unique_and_expired_uncommitted_views_are_reclaimed() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file.txt"), "content").unwrap();
        let views = tempfile::tempdir().unwrap();
        let owner = AgentId::new();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer(source.path(), views.path(), owner, 5)
            .unwrap();
        assert!(lease.mutable);
        assert_eq!(lease.backend, IsolationBackend::CopiedSnapshot);
        assert_eq!(
            fs::read_to_string(lease.view.join("file.txt")).unwrap(),
            "content"
        );
        assert!(matches!(
            coordinator.writer(source.path(), views.path(), owner, 10),
            Err(WorkspaceError::OwnerAlreadyHasWriter(id)) if id == owner
        ));
        assert_eq!(
            coordinator.recover_expired(5).unwrap(),
            vec![Recovery::ReclaimUncommitted { owner }]
        );
    }

    /// A repository with one commit, and the identity to address it by.
    fn repository() -> tempfile::TempDir {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet"]).unwrap();
        git(source.path(), ["config", "user.name", "ARSY Test"]).unwrap();
        git(
            source.path(),
            ["config", "user.email", "arsy@example.invalid"],
        )
        .unwrap();
        // Windows runners default `core.autocrlf` to true, which rewrites
        // line endings on checkout. These tests compare file contents byte
        // for byte to say whether a writer's work arrived, and that question
        // has nothing to do with Git's line-ending policy.
        git(source.path(), ["config", "core.autocrlf", "false"]).unwrap();
        fs::write(source.path().join("file.txt"), "base\n").unwrap();
        git(source.path(), ["add", "file.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "base"]).unwrap();
        source
    }

    fn owner() -> ViewOwner {
        ViewOwner {
            session: SessionId::new(),
            task: TaskId::new(),
            attempt: AttemptId::new(),
            agent: AgentId::new(),
        }
    }

    /// Commit `content` in a view and return the revision it produced.
    fn commit_in(view: &Path, content: &str) -> String {
        fs::write(view.join("file.txt"), content).unwrap();
        git(view, ["config", "user.name", "ARSY Test"]).unwrap();
        git(view, ["config", "user.email", "arsy@example.invalid"]).unwrap();
        git(view, ["commit", "--quiet", "-am", "writer"]).unwrap();
        git_revision(view).unwrap()
    }

    fn request<'a>(
        target: &'a Path,
        writer_revision: &'a str,
        base: &'a str,
        changed: &'a [String],
        validation: &'a [String],
    ) -> IntegrationRequest<'a> {
        IntegrationRequest {
            target,
            writer_revision,
            base_revision: base,
            changed_files: changed,
            already_integrated: &[],
            validation,
            unresolved: &[],
            expect_target_revision: None,
            allow_stale_base: false,
            policy_authorized: true,
        }
    }

    #[test]
    fn git_is_handed_a_drive_path_rather_than_an_extended_length_one() {
        // What `fs::canonicalize` returns on Windows, and what Git refuses:
        // `could not create leading directories of '//?/C:/…': Invalid
        // argument`. Asserted on every platform because the transform is a
        // string one, and the bug it fixes is only reachable on one.
        assert_eq!(
            plain(Path::new(r"\\?\C:\Users\runner\repo")),
            Path::new(r"C:\Users\runner\repo")
        );
        // A UNC path means something else, and the remainder after the
        // prefix is not a path on its own.
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\?\UNC\server\share")
        );
        // Everything else is already what Git wants.
        assert_eq!(plain(Path::new("/tmp/repo")), Path::new("/tmp/repo"));
    }

    #[test]
    fn two_writers_work_in_separate_views_and_both_reach_the_target() {
        let source = repository();
        fs::write(source.path().join("second.txt"), "base\n").unwrap();
        git(source.path(), ["add", "second.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "second"]).unwrap();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();

        let (first, second) = (owner(), owner());
        let one = coordinator
            .writer_for(source.path(), views.path(), &first, 10, DirtyPolicy::Refuse)
            .unwrap();
        let two = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &second,
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        assert_ne!(one.view, two.view, "each writer gets its own tree");
        assert_ne!(one.branch, two.branch, "and its own branch");

        let base = one.base_revision.clone();
        let one_head = commit_in(&one.view, "from one\n");
        fs::write(two.view.join("second.txt"), "from two\n").unwrap();
        git(&two.view, ["config", "user.name", "ARSY Test"]).unwrap();
        git(&two.view, ["config", "user.email", "arsy@example.invalid"]).unwrap();
        git(&two.view, ["commit", "--quiet", "-am", "writer two"]).unwrap();
        let two_head = git_revision(&two.view).unwrap();

        // An integrator applies both, in order, to the recorded target.
        let checks = vec![arsy_kernel::domain::ArtifactId::new().to_string()];
        let first_files = vec!["file.txt".to_owned()];
        assert!(matches!(
            coordinator
                .integrate(&request(
                    source.path(),
                    &one_head,
                    &base,
                    &first_files,
                    &checks
                ))
                .unwrap(),
            MergeOutcome::Applied { .. }
        ));
        let second_files = vec!["second.txt".to_owned()];
        assert!(matches!(
            coordinator
                .integrate(&IntegrationRequest {
                    already_integrated: &first_files,
                    ..request(source.path(), &two_head, &base, &second_files, &checks)
                })
                .unwrap(),
            MergeOutcome::Applied { .. }
        ));
        assert_eq!(
            fs::read_to_string(source.path().join("file.txt")).unwrap(),
            "from one\n"
        );
        assert_eq!(
            fs::read_to_string(source.path().join("second.txt")).unwrap(),
            "from two\n"
        );
    }

    #[test]
    fn integration_refuses_before_it_touches_the_target() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        let base = lease.base_revision.clone();
        let head = commit_in(&lease.view, "writer\n");
        let before = git_revision(source.path()).unwrap();
        let changed = vec!["file.txt".to_owned()];
        let checks = vec![arsy_kernel::domain::ArtifactId::new().to_string()];

        // Each refusal is its own reason, and none of them moves the target.
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                policy_authorized: false,
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::PolicyRequired)
        ));
        assert!(matches!(
            coordinator.integrate(&request(source.path(), &head, &base, &changed, &[])),
            Err(WorkspaceError::Unvalidated)
        ));
        assert!(matches!(
            coordinator.integrate(&request(
                source.path(),
                &head,
                &base,
                &changed,
                &["not-an-artifact".into()]
            )),
            Err(WorkspaceError::Unvalidated)
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                unresolved: &["a failing test".to_owned()],
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::Unresolved(_))
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                expect_target_revision: Some("0".repeat(40).as_str()),
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::TargetMoved { .. })
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                already_integrated: &changed,
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::PathOverlap(_))
        ));
        assert!(matches!(
            coordinator.integrate(&request(
                source.path(),
                &head,
                &"0".repeat(40),
                &changed,
                &checks
            )),
            Err(WorkspaceError::StaleBase { .. })
        ));
        assert_eq!(git_revision(source.path()).unwrap(), before);
        assert_eq!(
            fs::read_to_string(source.path().join("file.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn dirty_target_refusal_never_changes_the_index() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let lease = WorkspaceCoordinator::default()
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        let head = commit_in(&lease.view, "writer\n");
        let staged = source.path().join("already-staged.txt");
        fs::write(&staged, "stage only").unwrap();
        git(source.path(), ["add", "already-staged.txt"]).unwrap();
        let before = git_output(source.path(), ["ls-files", "--stage", "--", "."]).unwrap();
        fs::write(source.path().join("untracked.txt"), "leave untracked").unwrap();
        let changed = vec!["file.txt".into()];
        let checks = vec![ArtifactId::new().to_string()];
        assert!(matches!(
            WorkspaceCoordinator::default().integrate(&request(
                source.path(),
                &head,
                &lease.base_revision,
                &changed,
                &checks
            )),
            Err(WorkspaceError::DirtySource(_))
        ));
        assert_eq!(
            git_output(source.path(), ["ls-files", "--stage", "--", "."]).unwrap(),
            before
        );
        assert_eq!(
            git_output(
                source.path(),
                ["ls-files", "--others", "--exclude-standard"]
            )
            .unwrap(),
            "untracked.txt\n"
        );
    }

    #[test]
    fn refusing_an_untracked_source_does_not_stage_intent_to_add() {
        let source = repository();
        fs::write(source.path().join("untracked.txt"), "operator data").unwrap();
        let before = git_output(source.path(), ["ls-files", "--stage", "--", "."]).unwrap();
        let views = tempfile::tempdir().unwrap();
        assert!(matches!(
            WorkspaceCoordinator::default().writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse
            ),
            Err(WorkspaceError::DirtySource(_))
        ));
        assert_eq!(
            git_output(source.path(), ["ls-files", "--stage", "--", "."]).unwrap(),
            before
        );
    }

    #[test]
    fn committing_a_view_never_carries_harness_state() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let lease = WorkspaceCoordinator::default()
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        fs::write(lease.view.join("file.txt"), "validated\n").unwrap();
        fs::create_dir_all(lease.view.join(".arsy/state")).unwrap();
        fs::write(lease.view.join(".arsy/state/.gitignore"), "*\n").unwrap();
        fs::write(lease.view.join(".arsy/state/session"), "runtime log").unwrap();
        fs::write(lease.view.join(".arsy/config.toml"), "operator config").unwrap();
        let checked = crate::git::content_fingerprint(&lease.view).unwrap();
        let committed = WorkspaceCoordinator::commit_view(&lease, "writer")
            .unwrap()
            .unwrap();
        assert_eq!(
            checked,
            crate::git::committed_content_fingerprint(&lease.view, &committed).unwrap()
        );
        let files = git_output(&lease.view, ["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(files.contains(".arsy/config.toml\n"));
        assert!(!files.contains(".arsy/state/"), "{files}");
    }

    #[test]
    fn a_dirty_source_is_refused_or_carried_and_never_silently_dropped() {
        let source = repository();
        fs::write(source.path().join("file.txt"), "uncommitted\n").unwrap();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        assert!(matches!(
            coordinator.writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse
            ),
            Err(WorkspaceError::DirtySource(_))
        ));
        let carried = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::CarryPatch,
            )
            .unwrap();
        assert!(carried.carried_patch.is_some());
        assert_eq!(
            fs::read_to_string(carried.view.join("file.txt")).unwrap(),
            "uncommitted\n",
            "the operator's work reaches the view rather than vanishing"
        );
    }

    /// Another process can see a view is leased only through its marker, so
    /// the marker follows the lease from the moment the view is cut until it
    /// is discarded, and a shared reader view renews it on every use.
    #[test]
    fn a_lease_marker_follows_every_view_it_describes() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();

        let writer = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        assert_eq!(lease_expired(&writer.view, 9), Some(false));
        assert_eq!(lease_expired(&writer.view, 10), Some(true));
        coordinator.discard(&writer).unwrap();
        assert!(!lease_marker(&writer.view).exists());

        let reader = coordinator
            .reader(source.path(), views.path(), AgentId::new())
            .unwrap();
        assert_eq!(lease_expired(&reader.view, now_ms()), Some(false));

        // Removed from under this process, the shared view is cut again.
        let _ = git(
            source.path(),
            [
                "worktree",
                "remove",
                "--force",
                path_text(&reader.view).unwrap(),
            ],
        );
        let again = coordinator
            .reader(source.path(), views.path(), AgentId::new())
            .unwrap();
        assert!(again.view.is_dir());
        assert_eq!(lease_expired(&again.view, now_ms()), Some(false));
    }

    #[test]
    fn a_discarded_view_leaves_nothing_behind_but_keeps_committed_work() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let idle = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        coordinator.discard(&idle).unwrap();
        assert!(!idle.view.exists());
        assert!(!git_output(source.path(), ["branch", "--list"])
            .unwrap()
            .contains(idle.branch.as_deref().unwrap()));

        let worked = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        let head = commit_in(&worked.view, "kept\n");
        coordinator.discard(&worked).unwrap();
        assert!(!worked.view.exists());
        // The branch survives, because it is the only handle on what the
        // writer committed.
        assert!(git_output(source.path(), ["branch", "--list"])
            .unwrap()
            .contains(worked.branch.as_deref().unwrap()));
        assert!(git_output(source.path(), ["cat-file", "-t", &head]).is_ok());
    }

    #[test]
    fn git_writers_use_worktrees_and_merge_conflicts_return_evidence() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet"]).unwrap();
        git(source.path(), ["config", "user.name", "ARSY Test"]).unwrap();
        git(
            source.path(),
            ["config", "user.email", "arsy@example.invalid"],
        )
        .unwrap();
        fs::write(source.path().join("file.txt"), "base\n").unwrap();
        git(source.path(), ["add", "file.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "base"]).unwrap();

        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer(source.path(), views.path(), AgentId::new(), 10)
            .unwrap();
        assert_eq!(lease.backend, IsolationBackend::GitWorktree);
        fs::write(source.path().join("file.txt"), "target\n").unwrap();
        git(source.path(), ["commit", "--quiet", "-am", "target"]).unwrap();
        fs::write(lease.view.join("file.txt"), "writer\n").unwrap();
        git(&lease.view, ["commit", "--quiet", "-am", "writer"]).unwrap();
        let writer_revision = git_revision(&lease.view).unwrap();

        assert!(matches!(
            coordinator.merge_git(source.path(), &writer_revision, false),
            Err(WorkspaceError::PolicyRequired)
        ));
        let MergeOutcome::Conflict { paths, evidence } = coordinator
            .merge_git(source.path(), &writer_revision, true)
            .unwrap()
        else {
            panic!("divergent edits must conflict");
        };
        assert_eq!(paths, vec![PathBuf::from("file.txt")]);
        assert!(!evidence.is_empty());
    }
}
