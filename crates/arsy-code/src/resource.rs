//! The one place a workspace path is resolved.
//!
//! Every file a tool touches goes through [`Workspace`], which owns a cap-std
//! [`Dir`] for the root. Confinement is therefore two independent checks: a
//! lexical one that refuses `..`, absolute paths, and prefixes, and the
//! syscall-level one cap-std performs on every operation, which is what stops a
//! symlink whose name looks local from resolving outside the tree. Neither
//! check alone is enough, and nothing else in the crate is allowed to open a
//! workspace path by another route.

use arsy_kernel::domain::{ResourceRef, ResourceRefError, StateVersion};
use cap_std::{ambient_authority, fs::Dir};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};

/// A workspace file resolved once for both policy and execution.
pub struct ResolvedFile {
    resource: ResourceRef,
    file: File,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub digest: StateVersion,
}

impl FileContent {
    /// Whether the bytes are text a model can be shown.
    ///
    /// A NUL byte is the cheap, near-certain marker of a binary file, and it is
    /// the same test the searcher uses, so a file is never text to one and
    /// binary to the other.
    pub fn is_binary(&self) -> bool {
        self.bytes.contains(&0) || std::str::from_utf8(&self.bytes).is_err()
    }
}

/// The harness's own state, kept out of listings, searches, and file finds.
pub const STATE_DIRECTORY: &str = ".arsy";

/// One entry of a directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub directory: bool,
    pub bytes: u64,
}

impl ResolvedFile {
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn into_file(self) -> File {
        self.file
    }

    pub fn read(self, max_bytes: u64) -> io::Result<FileContent> {
        use io::Read;

        let mut bytes = Vec::new();
        self.file
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds read limit",
            ));
        }
        let digest = arsy_kernel::domain::StateVersion::from_digest(Sha256::digest(&bytes).into());
        Ok(FileContent { bytes, digest })
    }
}

/// Read a file the harness itself resolved, outside any workspace.
///
/// The only caller is a `skill://` reference the session's own listing already
/// named: the operator's home declared that skill, so its `SKILL.md` lives
/// outside the workspace and [`Workspace`] would refuse the absolute path. A
/// path a model supplied never reaches here — confinement still decides every
/// other read.
pub fn read_declared(path: &Path, max_bytes: u64) -> io::Result<FileContent> {
    use io::Read;

    let mut bytes = Vec::new();
    File::open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "file exceeds read limit",
        ));
    }
    let digest = StateVersion::from_digest(Sha256::digest(&bytes).into());
    Ok(FileContent { bytes, digest })
}

/// A capability directory that confines all path resolution to one workspace.
pub struct Workspace {
    root: Dir,
    path: PathBuf,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = std::fs::canonicalize(root)?;
        Dir::open_ambient_dir(&path, ambient_authority()).map(|root| Self { root, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The workspace-relative resource a path names, without opening it.
    ///
    /// Writes need a name for a file that does not exist yet, which
    /// [`Self::resolve_file`] cannot give: it canonicalizes, and canonicalizing
    /// a missing path fails. The lexical check is the same one; the syscall
    /// check happens when cap-std performs the operation.
    fn relative(&self, path: impl AsRef<Path>) -> Result<PathBuf, ResolveError> {
        confined(path.as_ref())
    }
    /// Git's administration is never a model-editable workspace file.
    /// Canonicalizing the nearest existing path also catches aliases to
    /// `.git`, including a worktree's `.git` pointer.
    fn mutation_relative(&self, path: impl AsRef<Path>) -> Result<PathBuf, ResolveError> {
        let relative = self.relative(path)?;
        if is_git_metadata(&self.path) || is_git_metadata(&relative) {
            return Err(ResolveError::GitMetadata);
        }
        let mut existing = relative.as_path();
        loop {
            match self.root.canonicalize(existing) {
                Ok(canonical) => {
                    if is_git_metadata(&canonical) {
                        return Err(ResolveError::GitMetadata);
                    }
                    return Ok(relative);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    existing = existing.parent().unwrap_or(Path::new("."));
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(ResolveError::OutsideWorkspace);
                }
                Err(error) => return Err(ResolveError::Io(error)),
            }
        }
    }

    /// Read a whole file, bounded.
    pub fn read(
        &self,
        path: impl AsRef<Path>,
        max_bytes: u64,
    ) -> Result<FileContent, ResolveError> {
        Ok(self.resolve_file(path)?.read(max_bytes)?)
    }

    /// Replace a file's contents, creating it and any missing parent.
    ///
    /// Returns the digest of what was written, which is the precondition a
    /// later edit of the same file is checked against.
    pub fn write(
        &self,
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<StateVersion, ResolveError> {
        let relative = self.mutation_relative(path)?;
        if let Some(parent) = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.root.create_dir_all(parent)?;
        }
        self.root.write(&relative, bytes)?;
        Ok(StateVersion::from_digest(Sha256::digest(bytes).into()))
    }

    /// Whether a path names something that exists, without opening it.
    ///
    /// A confinement failure is not an existence question, so it answers
    /// `false` rather than propagating: a caller asking "was this here before I
    /// wrote it" gets an answer, and the write itself is what refuses a path
    /// outside the workspace.
    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        self.relative(path)
            .is_ok_and(|relative| self.root.metadata(relative).is_ok())
    }

    /// Create a file that must not already exist.
    pub fn create_new(
        &self,
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<StateVersion, ResolveError> {
        let relative = self.mutation_relative(path)?;
        if self.exists(&relative) {
            return Err(ResolveError::AlreadyExists);
        }
        self.write(&relative, bytes)
    }

    pub fn remove(&self, path: impl AsRef<Path>) -> Result<(), ResolveError> {
        let relative = self.mutation_relative(path)?;
        Ok(self.root.remove_file(&relative)?)
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), ResolveError> {
        let from = self.mutation_relative(from)?;
        let to = self.mutation_relative(to)?;
        if self.exists(&to) {
            return Err(ResolveError::AlreadyExists);
        }
        if let Some(parent) = to.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            self.root.create_dir_all(parent)?;
        }
        Ok(self.root.rename(&from, &self.root, &to)?)
    }

    /// One directory level, sorted, directories marked.
    ///
    /// A relative path of `.` lists the root, which is the only way to name it
    /// through a checker that refuses an empty path.
    ///
    /// The harness's own state directory is left out: `.arsy` holds the session
    /// store and the artifacts of the very calls being made, so listing it
    /// shows the model its own exhaust and invites it to read or edit that
    /// instead of the project.
    pub fn list(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>, ResolveError> {
        let path = path.as_ref();
        let root_level = path.as_os_str().is_empty() || path == Path::new(".");
        let entries = if root_level {
            self.root.entries()?
        } else {
            self.root.read_dir(self.relative(path)?)?
        };
        let mut listed = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if root_level && name == STATE_DIRECTORY {
                continue;
            }
            let metadata = entry.metadata()?;
            listed.push(DirEntry {
                name,
                directory: metadata.is_dir(),
                bytes: metadata.len(),
            });
        }
        listed.sort_by(|left, right| {
            right
                .directory
                .cmp(&left.directory)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(listed)
    }

    pub fn resolve_file(&self, path: impl AsRef<Path>) -> Result<ResolvedFile, ResolveError> {
        let path = confined(path.as_ref())?;
        let canonical = self.root.canonicalize(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied {
                ResolveError::OutsideWorkspace
            } else {
                ResolveError::Io(error)
            }
        })?;
        let canonical = confined(&canonical)?;
        let file = self.root.open(&canonical)?.into_std();
        let value = canonical
            .to_str()
            .ok_or(ResolveError::NonUtf8)?
            .replace('\\', "/");

        Ok(ResolvedFile {
            resource: ResourceRef::new("workspace", value)?,
            file,
        })
    }
}

/// Where a path the model wrote lives, relative to what this session may touch.
pub enum Location {
    /// A plain workspace-relative path; [`Workspace`] confines it as always.
    Inside,
    /// Under the workspace or an added directory, named absolutely or with
    /// `..`: that root opened, and the path relative to it. Confinement then
    /// applies inside the root exactly as it does inside the workspace, so a
    /// symlink there still cannot leave it.
    Rooted(Workspace, PathBuf),
    /// Outside every root. Only an operator's approval of this exact path
    /// opens it, which is the caller's decision, not this module's.
    Undeclared(PathBuf),
}

/// Classify a path against the workspace and the added directories.
///
/// A path that names nothing outside lexically is [`Location::Inside`]. The
/// rest is made absolute against the workspace, normalized lexically, and
/// matched against the roots by whole components, so `../b-evil` is not under
/// `../b`.
pub fn locate(
    roots: &[PathBuf],
    workspace: &Path,
    path: impl AsRef<Path>,
) -> Result<Location, ResolveError> {
    let path = path.as_ref();
    if !escapes(path) {
        return Ok(Location::Inside);
    }
    let absolute = absolute(workspace, path);
    let Some(root) = root_of(roots, workspace, &absolute) else {
        return Ok(Location::Undeclared(absolute));
    };
    let relative = absolute
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let relative = if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    };
    Ok(Location::Rooted(Workspace::open(root)?, relative))
}

/// Whether a path lies outside the workspace and every added directory,
/// decided lexically and without touching the filesystem — the question
/// policy asks before a call runs.
pub fn is_undeclared(roots: &[PathBuf], workspace: &Path, path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    escapes(path) && root_of(roots, workspace, &absolute(workspace, path)).is_none()
}

/// The workspace or added directory an absolute path lies under.
fn root_of<'a>(roots: &'a [PathBuf], workspace: &'a Path, absolute: &Path) -> Option<&'a Path> {
    std::iter::once(workspace)
        .chain(roots.iter().map(PathBuf::as_path))
        .find(|root| absolute.starts_with(root))
}

/// A path made absolute against the workspace and normalized lexically.
pub fn absolute(workspace: &Path, path: impl AsRef<Path>) -> PathBuf {
    let mut absolute = PathBuf::new();
    for component in workspace.join(path).components() {
        match component {
            Component::ParentDir => {
                absolute.pop();
            }
            Component::CurDir => {}
            other => absolute.push(other),
        }
    }
    absolute
}

/// Open an approved path outside every root: the directory itself when
/// `directory`, otherwise its parent with the file name under it.
///
/// The operator approved the path they were shown, so a directory that
/// resolves somewhere else — a symlink anywhere along it — is refused and the
/// real path named, rather than opened: approving `/tmp/link` must not list
/// whatever it points at. The file name still resolves through cap-std, so a
/// symlink named there cannot redirect the effect either.
pub fn open_outside(
    absolute: &Path,
    directory: bool,
) -> Result<(Workspace, PathBuf), ResolveError> {
    let (root, relative) = if directory {
        (absolute, PathBuf::from("."))
    } else {
        let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(ResolveError::EmptyPath);
        };
        (parent, PathBuf::from(name))
    };
    let real = std::fs::canonicalize(root)?;
    if real != root {
        return Err(ResolveError::Symlinked(real.join(&relative)));
    }
    Ok((Workspace::open(root)?, relative))
}

/// Whether a path names something outside the workspace lexically.
pub fn escapes(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// The one walk every repository-wide traversal uses.
///
/// Honours `.gitignore` and the other standard filters, and skips the
/// harness's own state directory — so a listing, a text search, and a file find
/// agree about what the workspace contains rather than each drawing its own
/// boundary.
pub fn walk(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(STATE_DIRECTORY))
        .build()
}

fn is_git_metadata(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name.to_str().is_some_and(|part| part.eq_ignore_ascii_case(".git"))
        )
    })
}

fn confined(path: &Path) -> Result<PathBuf, ResolveError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ResolveError::OutsideWorkspace)
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(ResolveError::EmptyPath);
    }
    Ok(relative)
}

#[derive(Debug)]
pub enum ResolveError {
    OutsideWorkspace,
    EmptyPath,
    NonUtf8,
    GitMetadata,
    AlreadyExists,
    /// An approved outside path resolves through a symlink to this one.
    Symlinked(PathBuf),
    Io(io::Error),
    Resource(ResourceRefError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace => formatter.write_str("path escapes the workspace"),
            Self::EmptyPath => formatter.write_str("path must name a workspace file"),
            Self::GitMetadata => {
                formatter.write_str("Git administration is not editable through file tools")
            }
            Self::NonUtf8 => formatter.write_str("canonical workspace path is not UTF-8"),
            Self::AlreadyExists => formatter.write_str("a file already exists at that path"),
            Self::Symlinked(real) => write!(
                formatter,
                "that path resolves through a symlink to {}; name that path instead",
                real.display()
            ),
            Self::Io(error) => error.fmt(formatter),
            Self::Resource(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<io::Error> for ResolveError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ResourceRefError> for ResolveError {
    fn from(value: ResourceRefError) -> Self {
        Self::Resource(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn resolves_a_workspace_file_to_an_open_handle() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "original").unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();

        let resolved = workspace.resolve_file("src/./lib.rs").unwrap();
        let mut text = String::new();
        (&resolved.file).read_to_string(&mut text).unwrap();

        assert_eq!(resolved.resource.value(), "src/lib.rs");
        assert_eq!(text, "original");
    }

    #[test]
    fn file_mutations_never_touch_git_administration() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/config"), "protected").unwrap();
        std::fs::write(temp.path().join("source"), "kept").unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();
        for path in [".git", ".git/config", "./.git/config", ".GIT/config"] {
            assert!(matches!(
                workspace.write(path, b"forbidden"),
                Err(ResolveError::GitMetadata)
            ));
            assert!(matches!(
                workspace.create_new(path, b"forbidden"),
                Err(ResolveError::GitMetadata)
            ));
            assert!(matches!(
                workspace.remove(path),
                Err(ResolveError::GitMetadata)
            ));
        }
        assert!(matches!(
            workspace.rename(".git/config", "stolen"),
            Err(ResolveError::GitMetadata)
        ));
        assert!(matches!(
            workspace.rename("source", ".git/moved"),
            Err(ResolveError::GitMetadata)
        ));
        let admin = Workspace::open(temp.path().join(".git")).unwrap();
        assert!(matches!(
            admin.write("config", b"forbidden"),
            Err(ResolveError::GitMetadata)
        ));
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git/config")).unwrap(),
            "protected"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("source")).unwrap(),
            "kept"
        );
        workspace.write(".gitignore", b"safe").unwrap();
        assert_eq!(
            std::fs::read(temp.path().join(".gitignore")).unwrap(),
            b"safe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_mutations_cannot_alias_git_administration_through_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/config"), "protected").unwrap();
        symlink(".git", temp.path().join("alias")).unwrap();
        symlink(".git/config", temp.path().join("config-link")).unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();
        for path in ["alias/config", "alias/new", "config-link"] {
            assert!(matches!(
                workspace.write(path, b"forbidden"),
                Err(ResolveError::GitMetadata)
            ));
        }
        assert!(matches!(
            workspace.remove("alias/config"),
            Err(ResolveError::GitMetadata)
        ));
        assert!(matches!(
            workspace.rename("alias/config", "stolen"),
            Err(ResolveError::GitMetadata)
        ));
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git/config")).unwrap(),
            "protected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn execution_keeps_the_handle_that_policy_inspected() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file"), "approved").unwrap();
        let resolved = Workspace::open(temp.path())
            .unwrap()
            .resolve_file("file")
            .unwrap();

        std::fs::rename(temp.path().join("file"), temp.path().join("moved")).unwrap();
        std::fs::write(temp.path().join("file"), "replacement").unwrap();
        let mut text = String::new();
        (&resolved.file).read_to_string(&mut text).unwrap();

        assert_eq!(text, "approved");
    }

    #[test]
    fn rejects_parent_and_absolute_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();

        assert!(matches!(
            workspace.resolve_file("../outside"),
            Err(ResolveError::OutsideWorkspace)
        ));
        assert!(matches!(
            workspace.resolve_file(temp.path()),
            Err(ResolveError::OutsideWorkspace)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_an_internal_symlink_to_its_canonical_resource() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("target"), "inside").unwrap();
        symlink("target", temp.path().join("link")).unwrap();
        let resolved = Workspace::open(temp.path())
            .unwrap()
            .resolve_file("link")
            .unwrap();

        assert_eq!(resolved.resource.value(), "target");
    }

    fn rooted(location: Location) -> (Workspace, PathBuf) {
        match location {
            Location::Rooted(root, relative) => (root, relative),
            Location::Inside => panic!("expected a rooted path, got Inside"),
            Location::Undeclared(path) => panic!("expected a rooted path, got {path:?}"),
        }
    }

    #[test]
    fn locates_added_directories_and_nothing_beside_them() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("project-a");
        let added = parent.path().join("project-b");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(added.join("src")).unwrap();
        std::fs::write(added.join("src/lib.rs"), "added").unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let roots = vec![std::fs::canonicalize(&added).unwrap()];

        let (root, relative) =
            rooted(locate(&roots, &workspace, "../project-b/src/lib.rs").unwrap());
        assert_eq!(root.read(&relative, 64).unwrap().bytes, b"added");
        let (_, relative) =
            rooted(locate(&roots, &workspace, roots[0].join("src/lib.rs")).unwrap());
        assert_eq!(relative, Path::new("src/lib.rs"));
        let (_, relative) = rooted(locate(&roots, &workspace, "../project-b").unwrap());
        assert_eq!(relative, Path::new("."));
        let (root, relative) = rooted(locate(&roots, &workspace, workspace.join("x")).unwrap());
        assert_eq!(
            (root.path(), relative.as_path()),
            (workspace.as_path(), Path::new("x"))
        );

        assert!(matches!(
            locate(&roots, &workspace, "src/lib.rs"),
            Ok(Location::Inside)
        ));
        for outside in ["../private/x", "../project-b-evil/x"] {
            assert!(matches!(
                locate(&roots, &workspace, outside),
                Ok(Location::Undeclared(path)) if path.ends_with(&outside[3..])
            ));
        }
        assert!(matches!(
            locate(&[], &workspace, "../project-b/src/lib.rs"),
            Ok(Location::Undeclared(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_added_directory_symlink_cannot_leave_it() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let added = parent.path().join("project-b");
        std::fs::create_dir_all(&added).unwrap();
        std::fs::write(parent.path().join("secret"), "secret").unwrap();
        symlink(parent.path().join("secret"), added.join("escape")).unwrap();
        let roots = vec![std::fs::canonicalize(&added).unwrap()];

        let (root, relative) =
            rooted(locate(&roots, parent.path(), roots[0].join("escape")).unwrap());
        assert!(matches!(
            root.resolve_file(relative),
            Err(ResolveError::OutsideWorkspace)
        ));
    }

    #[test]
    fn an_approved_outside_path_opens_under_its_parent() {
        let parent = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(parent.path()).unwrap();
        let file = parent.join("notes.txt");
        std::fs::write(&file, "outside").unwrap();

        let (root, name) = open_outside(&file, false).unwrap();
        assert_eq!(root.read(&name, 64).unwrap().bytes, b"outside");
        let (root, here) = open_outside(&parent, true).unwrap();
        assert!(root
            .list(here)
            .unwrap()
            .iter()
            .any(|entry| entry.name == "notes.txt"));
        assert!(open_outside(Path::new("/"), false).is_err());
    }

    /// Approving `/tmp/link` approves that path, not whatever it points at.
    #[cfg(unix)]
    #[test]
    fn an_approved_outside_symlink_is_refused_with_its_real_path() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(parent.path()).unwrap();
        std::fs::create_dir(parent.join("secret")).unwrap();
        std::fs::write(parent.join("secret/key"), "key").unwrap();
        symlink(parent.join("secret"), parent.join("link")).unwrap();

        for (path, directory) in [
            (parent.join("link"), true),
            (parent.join("link/key"), false),
        ] {
            match open_outside(&path, directory) {
                Err(ResolveError::Symlinked(real)) => {
                    assert!(real.starts_with(parent.join("secret")), "{real:?}")
                }
                Err(other) => panic!("{path:?}: {other}"),
                Ok(_) => panic!("{path:?} was opened through the link"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();

        assert!(matches!(
            Workspace::open(temp.path()).unwrap().resolve_file("escape"),
            Err(ResolveError::OutsideWorkspace)
        ));
    }
}
