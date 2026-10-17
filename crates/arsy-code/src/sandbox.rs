//! Capability-to-platform sandbox plans. Missing mandatory controls fail closed.

use arsy_kernel::{
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    policy::SandboxAssurance,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

const MAX_WRITABLE_PATHS: usize = 64;
const MAX_MEMORY_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_PIDS: u32 = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Mechanisms {
    pub namespaces: bool,
    pub seccomp: bool,
    pub cgroups: bool,
    pub landlock: bool,
    pub seatbelt: bool,
    pub restricted_token: bool,
    pub job_objects: bool,
    pub acl: bool,
    pub network_filter: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SandboxLimits {
    pub memory_bytes: u64,
    pub cpu_percent: u16,
    pub max_pids: u32,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct TargetDescriptor {
    pub workspace: PathBuf,
    pub program: String,
    pub network_targets: Vec<ResourceRef>,
    pub limits: SandboxLimits,
    pub required_assurance: SandboxAssurance,
    pub worker: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SandboxPlan {
    pub platform: Platform,
    pub assurance: SandboxAssurance,
    pub workspace: PathBuf,
    pub program: String,
    pub writable_paths: Vec<PathBuf>,
    pub network_allowed: bool,
    pub limits: SandboxLimits,
    pub launcher: PathBuf,
    pub arguments: Vec<String>,
    pub profile: Option<String>,
}

impl SandboxPlan {
    pub fn command(&self, program: &str, args: &[String]) -> Command {
        let mut command = Command::new(&self.launcher);
        command
            .args(&self.arguments)
            .arg("--")
            .arg(program)
            .args(args);
        command
    }
}

/// Turn an attenuated file-write scope into paths the OS sandbox can enforce.
/// The grant may contain both parent and child patterns; intersect them by
/// path containment instead of unioning them into a wider writer directory.
fn writable_paths(
    grants: &[CapabilityGrant],
    workspace: &Path,
) -> Result<Vec<PathBuf>, SandboxError> {
    let mut writable = Vec::new();
    for grant in grants
        .iter()
        .filter(|grant| grant.action == CapabilityAction::FsWrite && !grant.is_expired(now_ms()))
    {
        if let Some(path) = write_scope_intersection(grant.scope.patterns(), workspace)? {
            if !writable.contains(&path) {
                writable.push(path);
            }
        }
    }
    Ok(writable)
}

fn write_scope_intersection(
    patterns: &[arsy_kernel::capability::ResourcePattern],
    workspace: &Path,
) -> Result<Option<PathBuf>, SandboxError> {
    let mut intersection: Option<PathBuf> = None;
    for pattern in patterns {
        if pattern.scheme() != "file" {
            return Ok(None);
        }
        let glob = pattern.glob();
        let prefix = if glob == "**" {
            ""
        } else if let Some(prefix) = glob.strip_suffix("/**") {
            prefix
        } else if glob
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
        {
            return Err(SandboxError::NonConcreteWriteScope);
        } else {
            glob
        };
        let requested = Path::new(prefix);
        let candidate = if prefix.is_empty() {
            workspace.to_path_buf()
        } else if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace.join(requested)
        };
        let candidate = candidate
            .canonicalize()
            .map_err(|_| SandboxError::InvalidWritablePath(candidate.clone()))?;
        if !candidate.starts_with(workspace) {
            return Err(SandboxError::InvalidWritablePath(candidate));
        }
        intersection = Some(match intersection {
            None => candidate,
            Some(current) if candidate.starts_with(&current) => candidate,
            Some(current) if current.starts_with(&candidate) => current,
            Some(_) => return Ok(None),
        });
    }
    Ok(intersection)
}

pub struct PlatformSandbox {
    platform: Platform,
    mechanisms: Mechanisms,
}

impl PlatformSandbox {
    pub const fn new(platform: Platform, mechanisms: Mechanisms) -> Self {
        Self {
            platform,
            mechanisms,
        }
    }

    pub fn detect() -> Result<Self, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            return Ok(Self::new(
                Platform::Linux,
                Mechanisms {
                    namespaces: executable("bwrap"),
                    seccomp: true,
                    cgroups: Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
                        && executable("systemd-run"),
                    landlock: Path::new("/sys/kernel/security").exists(),
                    ..Mechanisms::default()
                },
            ));
        }
        #[cfg(target_os = "macos")]
        {
            return Ok(Self::new(
                Platform::Macos,
                Mechanisms {
                    seatbelt: Path::new("/usr/bin/sandbox-exec").is_file(),
                    ..Mechanisms::default()
                },
            ));
        }
        #[cfg(target_os = "windows")]
        {
            return Ok(Self::new(
                Platform::Windows,
                Mechanisms {
                    restricted_token: true,
                    job_objects: true,
                    acl: true,
                    // Restricted tokens do not imply network isolation.
                    network_filter: false,
                    ..Mechanisms::default()
                },
            ));
        }
        #[allow(unreachable_code)]
        Err(SandboxError::UnsupportedPlatform)
    }

    pub fn assurance(&self) -> SandboxAssurance {
        match self.platform {
            Platform::Linux
                if self.mechanisms.namespaces
                    && self.mechanisms.seccomp
                    && self.mechanisms.cgroups
                    && self.mechanisms.landlock =>
            {
                SandboxAssurance::Full
            }
            Platform::Linux if self.mechanisms.namespaces => SandboxAssurance::Filesystem,
            Platform::Macos if self.mechanisms.seatbelt => SandboxAssurance::Filesystem,
            Platform::Windows
                if self.mechanisms.restricted_token
                    && self.mechanisms.job_objects
                    && self.mechanisms.acl
                    && self.mechanisms.network_filter =>
            {
                SandboxAssurance::Full
            }
            Platform::Windows
                if self.mechanisms.restricted_token
                    && self.mechanisms.job_objects
                    && self.mechanisms.acl =>
            {
                SandboxAssurance::Filesystem
            }
            _ => SandboxAssurance::None,
        }
    }

    pub fn compile(
        &self,
        grants: &[CapabilityGrant],
        target: &TargetDescriptor,
    ) -> Result<SandboxPlan, SandboxError> {
        let mut target = target.clone();
        target.workspace = target
            .workspace
            .canonicalize()
            .map_err(|_| SandboxError::InvalidTarget)?;
        validate_target(&target)?;
        let home = host_home()?;
        let process = ResourceRef::new("process", &target.program)
            .map_err(|_| SandboxError::InvalidTarget)?;
        if !grants.iter().any(|grant| {
            grant.action == CapabilityAction::ProcessExec
                && grant.scope.admits(&process)
                && !grant.is_expired(now_ms())
        }) {
            return Err(SandboxError::MissingProcessGrant);
        }
        let writable_paths = writable_paths(grants, &target.workspace)?;
        if writable_paths.len() > MAX_WRITABLE_PATHS {
            return Err(SandboxError::TooManyWritablePaths);
        }
        let network_allowed = !target.network_targets.is_empty();
        let network_granted = target.network_targets.iter().all(|resource| {
            grants.iter().any(|grant| {
                grant.action == CapabilityAction::NetworkConnect
                    && grant.scope.admits(resource)
                    && !grant.is_expired(now_ms())
            })
        });
        if network_allowed && !network_granted {
            return Err(SandboxError::MissingNetworkGrant);
        }
        let assurance = self.assurance();
        if assurance < target.required_assurance {
            return Err(SandboxError::AssuranceUnavailable {
                required: target.required_assurance,
                achieved: assurance,
            });
        }

        match self.platform {
            Platform::Linux => {
                self.linux_plan(&target, writable_paths, network_allowed, assurance, &home)
            }
            Platform::Macos => {
                self.macos_plan(&target, writable_paths, network_allowed, assurance, &home)
            }
            Platform::Windows => {
                self.windows_plan(&target, writable_paths, network_allowed, assurance)
            }
        }
    }

    fn linux_plan(
        &self,
        target: &TargetDescriptor,
        writable_paths: Vec<PathBuf>,
        network_allowed: bool,
        assurance: SandboxAssurance,
        home: &Path,
    ) -> Result<SandboxPlan, SandboxError> {
        if !self.mechanisms.namespaces {
            return Err(SandboxError::BackendUnavailable("bwrap"));
        }
        let workspace_mount = Path::new("/workspace");
        let writable_root = writable_paths.contains(&target.workspace);
        let mut arguments = vec![
            "--die-with-parent".into(),
            "--new-session".into(),
            "--unshare-user".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--unshare-uts".into(),
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--dir".into(),
            workspace_mount.display().to_string(),
            if writable_root {
                "--bind".into()
            } else {
                "--ro-bind".into()
            },
            target.workspace.display().to_string(),
            workspace_mount.display().to_string(),
        ];
        for path in writable_paths
            .iter()
            .filter(|path| **path != target.workspace)
        {
            let relative = path
                .strip_prefix(&target.workspace)
                .map_err(|_| SandboxError::InvalidWritablePath(path.clone()))?;
            arguments.extend([
                "--bind".into(),
                path.display().to_string(),
                workspace_mount.join(relative).display().to_string(),
            ]);
        }
        // A worktree's .git pointer names the shared repository metadata.
        // Keep that control file read-only even inside a writable view.
        let git_metadata = target.workspace.join(".git");
        if std::fs::symlink_metadata(&git_metadata).is_ok() {
            arguments.extend([
                "--ro-bind".into(),
                git_metadata.display().to_string(),
                workspace_mount.join(".git").display().to_string(),
            ]);
        }
        arguments.extend([
            "--dir".into(),
            "/arsy-runtime".into(),
            "--ro-bind".into(),
            target.worker.display().to_string(),
            "/arsy-runtime/arsy-sandbox-worker".into(),
        ]);
        if !network_allowed {
            arguments.push("--unshare-net".into());
        }
        let home = home.to_path_buf();
        let mut search_path = vec![PathBuf::from("/arsy-cache/bin")];
        if let Some(path) = std::env::var_os("PATH") {
            search_path
                .extend(std::env::split_paths(&path).filter(|entry| !entry.starts_with(&home)));
        }
        let search_path = std::env::join_paths(search_path)
            .map_err(|error| SandboxError::Serialization(error.to_string()))?;
        // Compiler executables and caches are mounted at a separate
        // read-only prefix. The original home, including credentials, is
        // hidden before the child process starts.
        for path in [
            PathBuf::from("/arsy-cache"),
            PathBuf::from("/arsy-cache/bin"),
            PathBuf::from("/arsy-cache/cargo"),
            PathBuf::from("/arsy-cache/cargo/registry"),
            PathBuf::from("/arsy-cache/cargo/git"),
            PathBuf::from("/arsy-cache/rustup"),
            PathBuf::from("/arsy-cache/rustup/toolchains"),
        ] {
            arguments.extend(["--dir".into(), path.display().to_string()]);
        }
        for (source, destination) in [
            (home.join(".cargo/bin"), PathBuf::from("/arsy-cache/bin")),
            (
                home.join(".cargo/registry"),
                PathBuf::from("/arsy-cache/cargo/registry"),
            ),
            (
                home.join(".cargo/git"),
                PathBuf::from("/arsy-cache/cargo/git"),
            ),
            (
                home.join(".rustup/toolchains"),
                PathBuf::from("/arsy-cache/rustup/toolchains"),
            ),
        ] {
            if source.exists() {
                arguments.extend([
                    "--ro-bind".into(),
                    source.display().to_string(),
                    destination.display().to_string(),
                ]);
            }
        }
        let settings = home.join(".rustup/settings.toml");
        if settings.is_file() {
            arguments.extend([
                "--ro-bind".into(),
                settings.display().to_string(),
                "/arsy-cache/rustup/settings.toml".into(),
            ]);
        }
        arguments.extend(["--tmpfs".into(), home.display().to_string()]);
        arguments.extend([
            "--tmpfs".into(),
            "/tmp".into(),
            "--dir".into(),
            "/tmp/arsy-home".into(),
            "--setenv".into(),
            "HOME".into(),
            "/tmp/arsy-home".into(),
            "--setenv".into(),
            "TMPDIR".into(),
            "/tmp".into(),
            "--setenv".into(),
            "PATH".into(),
            search_path.to_string_lossy().into_owned(),
            "--setenv".into(),
            "CARGO_HOME".into(),
            "/arsy-cache/cargo".into(),
            "--setenv".into(),
            "RUSTUP_HOME".into(),
            "/arsy-cache/rustup".into(),
            "--chdir".into(),
            workspace_mount.display().to_string(),
        ]);
        let mut sandbox_writable = Vec::with_capacity(writable_paths.len());
        for path in &writable_paths {
            let relative = path
                .strip_prefix(&target.workspace)
                .map_err(|_| SandboxError::InvalidWritablePath(path.clone()))?;
            sandbox_writable.push(if relative.as_os_str().is_empty() {
                workspace_mount.to_path_buf()
            } else {
                workspace_mount.join(relative)
            });
        }
        sandbox_writable.push(PathBuf::from("/tmp"));
        let controls = serde_json::json!({
            "limits": target.limits,
            "workspace": workspace_mount,
            "writable": sandbox_writable,
            "network_allowed": network_allowed,
        });
        assert!(controls["writable"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|path| path == "/tmp")));
        arguments.extend([
            "/arsy-runtime/arsy-sandbox-worker".into(),
            "--linux-controls".into(),
            serde_json::to_string(&controls)
                .map_err(|error| SandboxError::Serialization(error.to_string()))?,
        ]);
        let (launcher, arguments) = if assurance == SandboxAssurance::Full {
            let mut scoped = vec![
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--wait".into(),
                "--collect".into(),
                "-p".into(),
                format!("MemoryMax={}", target.limits.memory_bytes),
                "-p".into(),
                format!("CPUQuota={}%", target.limits.cpu_percent),
                "-p".into(),
                format!("TasksMax={}", target.limits.max_pids),
                "--".into(),
                "bwrap".into(),
            ];
            scoped.extend(arguments);
            (PathBuf::from("systemd-run"), scoped)
        } else {
            (PathBuf::from("bwrap"), arguments)
        };
        Ok(SandboxPlan {
            platform: Platform::Linux,
            assurance,
            workspace: target.workspace.clone(),
            program: target.program.clone(),
            writable_paths,
            network_allowed,
            limits: target.limits,
            launcher,
            arguments,
            profile: None,
        })
    }

    fn macos_plan(
        &self,
        target: &TargetDescriptor,
        writable_paths: Vec<PathBuf>,
        network_allowed: bool,
        assurance: SandboxAssurance,
        home: &Path,
    ) -> Result<SandboxPlan, SandboxError> {
        if !self.mechanisms.seatbelt {
            return Err(SandboxError::BackendUnavailable("sandbox-exec"));
        }
        let profile = seatbelt_profile_for_paths(
            &target.workspace,
            &writable_paths,
            network_allowed,
            Some(home),
            Some(&target.worker),
        )?;
        Ok(SandboxPlan {
            platform: Platform::Macos,
            assurance,
            workspace: target.workspace.clone(),
            program: target.program.clone(),
            writable_paths,
            network_allowed,
            limits: target.limits,
            launcher: target.worker.clone(),
            arguments: vec![
                "--limits".into(),
                serde_json::to_string(&target.limits)
                    .map_err(|error| SandboxError::Serialization(error.to_string()))?,
                "--seatbelt".into(),
                profile.clone(),
            ],
            profile: Some(profile),
        })
    }

    fn windows_plan(
        &self,
        target: &TargetDescriptor,
        writable_paths: Vec<PathBuf>,
        network_allowed: bool,
        assurance: SandboxAssurance,
    ) -> Result<SandboxPlan, SandboxError> {
        if assurance == SandboxAssurance::None {
            return Err(SandboxError::BackendUnavailable("Windows sandbox worker"));
        }
        if !network_allowed && !self.mechanisms.network_filter {
            return Err(SandboxError::BackendUnavailable("Windows network filter"));
        }
        let spec = serde_json::json!({
            "restricted_token": true,
            "job": {
                "memory_bytes": target.limits.memory_bytes,
                "cpu_percent": target.limits.cpu_percent,
                "max_pids": target.limits.max_pids,
                "kill_on_close": true
            },
            "workspace_acl": target.workspace,
            "writable_paths": writable_paths,
            "network_allowed": network_allowed
        });
        Ok(SandboxPlan {
            platform: Platform::Windows,
            assurance,
            workspace: target.workspace.clone(),
            program: target.program.clone(),
            writable_paths,
            network_allowed,
            limits: target.limits,
            launcher: target.worker.clone(),
            arguments: vec![
                "--windows-controls".into(),
                serde_json::to_string(&spec)
                    .map_err(|error| SandboxError::Serialization(error.to_string()))?,
            ],
            profile: None,
        })
    }
}

pub fn seatbelt_profile(
    workspace: &Path,
    writable_paths: &[PathBuf],
    network_allowed: bool,
) -> Result<String, SandboxError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    seatbelt_profile_for_paths(
        workspace,
        writable_paths,
        network_allowed,
        home.as_deref(),
        None,
    )
}

fn seatbelt_profile_for_paths(
    workspace: &Path,
    writable_paths: &[PathBuf],
    network_allowed: bool,
    home: Option<&Path>,
    worker: Option<&Path>,
) -> Result<String, SandboxError> {
    let workspace = workspace
        .canonicalize()
        .map_err(|_| SandboxError::InvalidTarget)?;
    let mut profile =
        String::from("(version 1)\n(deny default)\n(allow process-exec process-fork signal)\n");
    for path in [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/dev",
        "/private/etc",
        "/private/tmp",
        "/opt/homebrew",
        "/usr/local",
    ] {
        let path = Path::new(path);
        if path.exists() {
            profile.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                escaped_path(path)?
            ));
        }
    }
    // Dynamic language runtimes may need host system libraries; explicitly
    // deny the operator's home below, then restore only the isolated view.
    profile.push_str("(allow file-read* (subpath \"/\"))\n");
    if let Some(home) = home.filter(|home| home.is_absolute() && *home != Path::new("/")) {
        let home = home
            .canonicalize()
            .map_err(|_| SandboxError::InvalidTarget)?;
        profile.push_str(&format!(
            "(allow file-read-metadata (subpath \"{}\"))\n",
            escaped_path(&home)?
        ));
        let mut parent = home.parent();
        while let Some(path) = parent {
            profile.push_str(&format!(
                "(allow file-read-metadata (subpath \"{}\"))\n",
                escaped_path(path)?
            ));
            parent = path.parent();
        }
        profile.push_str(&format!(
            "(deny file-read* (subpath \"{}\"))\n",
            escaped_path(&home)?
        ));
        for relative in [
            ".cargo/bin",
            ".cargo/registry",
            ".cargo/git",
            ".rustup/toolchains",
            ".rustup/settings.toml",
        ] {
            let path = home.join(relative);
            if path.exists() {
                profile.push_str(&format!(
                    "(allow file-read* (subpath \"{}\"))\n",
                    escaped_path(&path)?
                ));
            }
        }
    }
    profile.push_str("(allow file-read* file-write* (subpath \"/private/tmp\"))\n");
    if let Some(worker) = worker {
        profile.push_str(&format!(
            "(allow file-read* (literal \"{}\"))\n",
            escaped_path(worker)?
        ));
    }
    profile.push_str(&format!(
        "(allow file-read* (subpath \"{}\"))\n",
        escaped_path(&workspace)?
    ));
    for path in writable_paths {
        let path = path
            .canonicalize()
            .map_err(|_| SandboxError::InvalidWritablePath(path.clone()))?;
        if !path.starts_with(&workspace) {
            return Err(SandboxError::InvalidWritablePath(path));
        }
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escaped_path(&path)?
        ));
    }
    let git_metadata = workspace.join(".git");
    profile.push_str(&format!(
        "(deny file-write* (subpath \"{}\"))\n",
        escaped_path(&git_metadata)?
    ));
    if network_allowed {
        profile.push_str("(allow network*)\n");
    } else {
        profile.push_str("(deny network*)\n");
    }
    validate_seatbelt_profile(&profile)?;
    Ok(profile)
}

/// Conformance entry point for macOS profile labs. Seatbelt profile language
/// version 1 is shared by supported releases, but every release still runs
/// through validation instead of being accepted as an unversioned profile.
pub fn seatbelt_profile_for_macos_major(
    macos_major: u16,
    workspace: &Path,
    writable_paths: &[PathBuf],
    network_allowed: bool,
) -> Result<String, SandboxError> {
    if macos_major < 13 {
        return Err(SandboxError::UnsupportedMacosProfile(macos_major));
    }
    seatbelt_profile(workspace, writable_paths, network_allowed)
}

pub fn validate_seatbelt_profile(profile: &str) -> Result<(), SandboxError> {
    let mut depth = 0_u32;
    let mut quoted = false;
    let mut escaped = false;
    for character in profile.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else {
            match character {
                '"' => quoted = true,
                '(' => depth = depth.checked_add(1).ok_or(SandboxError::InvalidProfile)?,
                ')' => depth = depth.checked_sub(1).ok_or(SandboxError::InvalidProfile)?,
                _ => {}
            }
        }
    }
    if quoted || depth != 0 || !profile.starts_with("(version 1)\n(deny default)\n") {
        return Err(SandboxError::InvalidProfile);
    }
    Ok(())
}

fn escaped_path(path: &Path) -> Result<String, SandboxError> {
    let value = path
        .to_str()
        .ok_or_else(|| SandboxError::InvalidWritablePath(path.to_owned()))?;
    if value.contains(['\0', '\n', '\r']) {
        return Err(SandboxError::InvalidWritablePath(path.to_owned()));
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn validate_target(target: &TargetDescriptor) -> Result<(), SandboxError> {
    if target.program.is_empty()
        || !target.workspace.is_absolute()
        || !target.workspace.is_dir()
        || !target.worker.is_absolute()
        || target.limits.memory_bytes == 0
        || target.limits.memory_bytes > MAX_MEMORY_BYTES
        || !(1..=10_000).contains(&target.limits.cpu_percent)
        || !(1..=MAX_PIDS).contains(&target.limits.max_pids)
        || target.limits.max_output_bytes == 0
    {
        return Err(SandboxError::InvalidTarget);
    }
    Ok(())
}

fn host_home() -> Result<PathBuf, SandboxError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(SandboxError::HomeUnavailable)?;
    let home = home
        .canonicalize()
        .map_err(|_| SandboxError::HomeUnavailable)?;
    if home == Path::new("/") {
        return Err(SandboxError::HomeUnavailable);
    }
    Ok(home)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(u64::MAX, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(target_os = "linux")]
fn executable(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(name).is_file())
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum SandboxError {
    UnsupportedPlatform,
    InvalidTarget,
    MissingProcessGrant,
    MissingNetworkGrant,
    NonConcreteWriteScope,
    InvalidWritablePath(PathBuf),
    TooManyWritablePaths,
    BackendUnavailable(&'static str),
    AssuranceUnavailable {
        required: SandboxAssurance,
        achieved: SandboxAssurance,
    },
    InvalidProfile,
    UnsupportedMacosProfile(u16),
    HomeUnavailable,
    Serialization(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => formatter.write_str("sandbox platform is unsupported"),
            Self::InvalidTarget => formatter.write_str("sandbox target or limits are invalid"),
            Self::MissingProcessGrant => formatter.write_str("process grant does not cover target"),
            Self::MissingNetworkGrant => formatter.write_str("network target is not granted"),
            Self::NonConcreteWriteScope => {
                formatter.write_str("writable sandbox mounts must be concrete paths")
            }
            Self::InvalidWritablePath(path) => {
                write!(
                    formatter,
                    "invalid writable sandbox path: {}",
                    path.display()
                )
            }
            Self::TooManyWritablePaths => formatter.write_str("too many writable sandbox paths"),
            Self::BackendUnavailable(name) => {
                write!(formatter, "sandbox backend unavailable: {name}")
            }
            Self::AssuranceUnavailable { required, achieved } => write!(
                formatter,
                "sandbox assurance {required:?} required, only {achieved:?} available"
            ),
            Self::InvalidProfile => formatter.write_str("generated Seatbelt profile is invalid"),
            Self::UnsupportedMacosProfile(version) => {
                write!(
                    formatter,
                    "macOS {version} has no validated Seatbelt profile"
                )
            }
            Self::HomeUnavailable => {
                formatter.write_str("operator home directory is unavailable to hide")
            }
            Self::Serialization(error) => {
                write!(formatter, "sandbox plan serialization failed: {error}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        capability::{PolicySource, ResourcePattern, ResourceScope},
        domain::{GrantId, Principal},
    };

    fn grant(action: CapabilityAction, scheme: &str, glob: String) -> CapabilityGrant {
        CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action,
            scope: ResourceScope::single(ResourcePattern::new(scheme, glob).unwrap()),
            expires_at_ms: None,
            delegation_depth: 0,
            source: PolicySource::User,
        }
    }

    fn target(root: &Path) -> TargetDescriptor {
        TargetDescriptor {
            workspace: root.to_owned(),
            program: "sh".into(),
            network_targets: Vec::new(),
            limits: SandboxLimits {
                memory_bytes: 512 * 1024 * 1024,
                cpu_percent: 100,
                max_pids: 32,
                max_output_bytes: 1024,
            },
            required_assurance: SandboxAssurance::Filesystem,
            worker: root.join("arsy-sandbox-worker"),
        }
    }

    #[test]
    fn writer_scope_intersects_parent_and_child_grants() {
        let temp = tempfile::tempdir().unwrap();
        let view = temp.path().join("view");
        std::fs::create_dir_all(view.join("src")).unwrap();
        let view = view.canonicalize().unwrap();
        let target = TargetDescriptor {
            workspace: view.clone(),
            ..target(&view)
        };
        let backend = PlatformSandbox::new(
            Platform::Macos,
            Mechanisms {
                seatbelt: true,
                ..Default::default()
            },
        );
        let parent_pattern =
            ResourcePattern::new("file", format!("{}/src/**", view.display())).unwrap();
        let requested_pattern = ResourcePattern::new("file", "**").unwrap();
        let write = CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action: CapabilityAction::FsWrite,
            scope: ResourceScope::new(vec![parent_pattern, requested_pattern]),
            expires_at_ms: None,
            delegation_depth: 0,
            source: PolicySource::User,
        };
        let grants = [
            grant(CapabilityAction::ProcessExec, "process", "sh".into()),
            write,
        ];
        let plan = backend.compile(&grants, &target).unwrap();
        assert_eq!(plan.writable_paths, [view.join("src")]);
        assert!(!plan.writable_paths.contains(&view));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_denies_home_reads_but_allows_the_committed_view() {
        use std::process::Command;

        let home = tempfile::tempdir().unwrap();
        let view = home.path().join("repo/.arsy/state/views/reader");
        std::fs::create_dir_all(&view).unwrap();
        let secret = home.path().join("credential");
        std::fs::write(view.join(".git"), "protected").unwrap();
        let visible = view.join("visible");
        std::fs::write(&secret, "private").unwrap();
        std::fs::write(&visible, "workspace").unwrap();
        let profile =
            seatbelt_profile_for_paths(&view, &[], false, Some(home.path()), None).unwrap();
        let output = Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "/bin/sh",
                "-c",
                &format!(
                    "cat '{}' > /dev/null && cat '{}'",
                    secret.display(),
                    visible.display()
                ),
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "the host credential must be unreadable"
        );

        let output = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/cat", visible.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the isolated workspace stays readable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"workspace");
        let write_profile = seatbelt_profile_for_paths(
            &view,
            std::slice::from_ref(&view),
            false,
            Some(home.path()),
            None,
        )
        .unwrap();
        let write = Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &write_profile,
                "/bin/sh",
                "-c",
                &format!("printf changed > '{}'", view.join(".git").display()),
            ])
            .output()
            .unwrap();
        assert!(
            !write.status.success(),
            "the worktree control file remains read-only"
        );

        assert_eq!(std::fs::read(view.join(".git")).unwrap(), b"protected");
    }
    #[test]
    fn linux_plan_is_default_deny_and_has_explicit_writes_and_limits() {
        let temp = tempfile::tempdir().unwrap();
        let target = target(temp.path());
        let backend = PlatformSandbox::new(
            Platform::Linux,
            Mechanisms {
                namespaces: true,
                seccomp: true,
                cgroups: true,
                landlock: true,
                ..Mechanisms::default()
            },
        );
        let plan = backend
            .compile(
                &[
                    grant(CapabilityAction::ProcessExec, "process", "sh".into()),
                    grant(
                        CapabilityAction::FsWrite,
                        "file",
                        format!("{}/**", temp.path().display()),
                    ),
                ],
                &target,
            )
            .unwrap();

        assert_eq!(plan.assurance, SandboxAssurance::Full);
        assert!(!plan.network_allowed);
        assert!(plan.arguments.contains(&"--unshare-net".to_owned()));
        assert_eq!(
            plan.writable_paths,
            vec![temp.path().canonicalize().unwrap()]
        );
        assert!(plan.arguments.contains(&"/workspace".to_owned()));
        let controls = plan
            .arguments
            .iter()
            .find(|argument| argument.starts_with("{\"limits\""))
            .expect("worker receives scoped controls");
        let controls: serde_json::Value = serde_json::from_str(controls).unwrap();
        assert_eq!(controls["workspace"], "/workspace");
        assert_eq!(
            controls["writable"],
            serde_json::json!(["/workspace", "/tmp"])
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert!(plan
                .arguments
                .windows(2)
                .any(|pair| { pair[0] == "--tmpfs" && pair[1] == home.display().to_string() }));
        }
        assert!(plan
            .arguments
            .windows(2)
            .any(|pair| pair[0] == "--setenv" && pair[1] == "HOME"));
        assert_eq!(plan.limits.max_pids, 32);
    }

    #[test]
    fn macos_profile_is_generated_escaped_and_version_validated() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("a\"b");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let profile = seatbelt_profile(&workspace, &[], false).unwrap();
        assert!(
            !profile.contains("(allow file-write*"),
            "readers must stay read-only"
        );
        assert!(profile.contains("(allow file-read* (subpath \"/\"))"));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert!(profile.contains(&format!(
                "(deny file-read* (subpath \"{}\"))",
                escaped_path(&home.canonicalize().unwrap()).unwrap()
            )));
        }
        let writer = seatbelt_profile(&workspace, std::slice::from_ref(&workspace), false).unwrap();
        assert!(writer.contains(&escaped_path(&workspace).unwrap()));
        assert!(writer.contains(&format!(
            "(deny file-write* (subpath \"{}/.git\"))",
            escaped_path(&workspace).unwrap()
        )));
        assert_eq!(
            seatbelt_profile(&workspace, &[PathBuf::from("/tmp/outside")], false),
            Err(SandboxError::InvalidWritablePath(PathBuf::from(
                "/tmp/outside"
            )))
        );
        assert!(!profile.contains("allow network"));
        assert_eq!(
            validate_seatbelt_profile("(version 1)\n(deny default)\n("),
            Err(SandboxError::InvalidProfile)
        );
        let versioned = temp.path().join("versioned");
        std::fs::create_dir(&versioned).unwrap();
        for major in [13, 14, 15, 26] {
            assert!(seatbelt_profile_for_macos_major(major, &versioned, &[], false).is_ok());
        }
        assert_eq!(
            seatbelt_profile_for_macos_major(12, &versioned, &[], false),
            Err(SandboxError::UnsupportedMacosProfile(12))
        );
    }

    #[test]
    fn windows_reports_weaker_assurance_without_a_network_filter() {
        let backend = PlatformSandbox::new(
            Platform::Windows,
            Mechanisms {
                restricted_token: true,
                job_objects: true,
                acl: true,
                ..Mechanisms::default()
            },
        );
        assert_eq!(backend.assurance(), SandboxAssurance::Filesystem);
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            backend.compile(
                &[grant(CapabilityAction::ProcessExec, "process", "sh".into())],
                &target(temp.path())
            ),
            Err(SandboxError::BackendUnavailable("Windows network filter"))
        ));
    }

    #[test]
    fn missing_controls_fail_closed_instead_of_returning_an_unsandboxed_plan() {
        let temp = tempfile::tempdir().unwrap();
        let backend = PlatformSandbox::new(Platform::Linux, Mechanisms::default());
        assert!(matches!(
            backend.compile(
                &[grant(CapabilityAction::ProcessExec, "process", "sh".into())],
                &target(temp.path())
            ),
            Err(SandboxError::AssuranceUnavailable { .. })
        ));
    }
}
