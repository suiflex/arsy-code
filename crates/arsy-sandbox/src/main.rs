//! Small, separately-auditable process that applies last-mile sandbox controls.

#![deny(unsafe_code)]

use serde::Deserialize;
use std::{
    env,
    process::{self, Command},
};

#[derive(Clone, Copy, Debug, Deserialize)]
struct Limits {
    #[cfg_attr(windows, allow(dead_code))]
    memory_bytes: u64,
    #[allow(dead_code)]
    cpu_percent: u16,
    #[allow(dead_code)]
    max_pids: u32,
    #[cfg_attr(windows, allow(dead_code))]
    max_output_bytes: u64,
}

#[derive(Debug)]
struct WorkerArgs {
    limits: Limits,
    seatbelt: Option<String>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    linux: Option<LinuxControls>,
    #[cfg_attr(not(windows), allow(dead_code))]
    windows: Option<WindowsControls>,
    program: String,
    arguments: Vec<String>,
}

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct LinuxControls {
    workspace: String,
    writable: Vec<String>,
    network_allowed: bool,
    max_pids: u32,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
struct WindowsControls {
    restricted_token: bool,
    job: WindowsJob,
    workspace_acl: String,
    writable_paths: Vec<String>,
    network_allowed: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
struct WindowsJob {
    memory_bytes: u64,
    cpu_percent: u16,
    max_pids: u32,
    kill_on_close: bool,
}

fn main() {
    match parse(env::args().skip(1).collect())
        .and_then(|args| apply_limits(args.limits).map(|()| args))
        .and_then(run)
    {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("ARSY-SBX-1001: {error}");
            process::exit(74);
        }
    }
}

fn parse(arguments: Vec<String>) -> Result<WorkerArgs, String> {
    let divider = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "worker command separator is missing".to_owned())?;
    let (options, command) = arguments.split_at(divider);
    let (program, arguments) = command[1..]
        .split_first()
        .ok_or_else(|| "worker target is missing".to_owned())?;
    let mut limits = None;
    let mut seatbelt = None;
    let mut linux = None;
    let mut windows = None;
    let mut index = 0;
    while index < options.len() {
        let value = options
            .get(index + 1)
            .ok_or_else(|| format!("{} needs a value", options[index]))?;
        match options[index].as_str() {
            "--limits" => {
                limits = Some(serde_json::from_str(value).map_err(|error| error.to_string())?)
            }
            "--seatbelt" => seatbelt = Some(value.clone()),
            "--linux-controls" => {
                let raw: serde_json::Value =
                    serde_json::from_str(value).map_err(|error| error.to_string())?;
                limits = Some(
                    serde_json::from_value(raw["limits"].clone())
                        .map_err(|error| error.to_string())?,
                );
                linux = Some(LinuxControls {
                    workspace: raw["workspace"]
                        .as_str()
                        .ok_or_else(|| "Linux workspace is missing".to_owned())?
                        .to_owned(),
                    writable: serde_json::from_value(raw["writable"].clone())
                        .map_err(|error| error.to_string())?,
                    network_allowed: raw["network_allowed"]
                        .as_bool()
                        .ok_or_else(|| "Linux network mode is missing".to_owned())?,
                    max_pids: raw["limits"]["max_pids"]
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| "Linux process limit is missing".to_owned())?,
                });
            }
            "--windows-controls" => {
                let controls: WindowsControls =
                    serde_json::from_str(value).map_err(|error| error.to_string())?;
                limits = Some(Limits {
                    memory_bytes: controls.job.memory_bytes,
                    cpu_percent: controls.job.cpu_percent,
                    max_pids: controls.job.max_pids,
                    max_output_bytes: u64::MAX,
                });
                windows = Some(controls);
            }
            option => return Err(format!("unknown worker option {option}")),
        }
        index += 2;
    }
    Ok(WorkerArgs {
        limits: limits.ok_or_else(|| "worker limits are missing".to_owned())?,
        seatbelt,
        linux,
        windows,
        program: program.clone(),
        arguments: arguments.to_vec(),
    })
}

#[cfg(unix)]
fn apply_limits(limits: Limits) -> Result<(), String> {
    use rlimit::Resource;

    #[cfg(target_os = "macos")]
    let _ = limits.memory_bytes;
    #[cfg(target_os = "linux")]
    Resource::AS
        .set(limits.memory_bytes, limits.memory_bytes)
        .map_err(|error| format!("memory limit failed: {error}"))?;
    Resource::FSIZE
        .set(limits.max_output_bytes, limits.max_output_bytes)
        .map_err(|error| format!("file-size limit failed: {error}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_limits(_limits: Limits) -> Result<(), String> {
    Ok(())
}

fn run(args: WorkerArgs) -> Result<i32, String> {
    #[cfg(target_os = "linux")]
    if let Some(controls) = args.linux {
        linux::restrict(&controls)?;
    }

    #[cfg(windows)]
    if let Some(controls) = args.windows {
        return windows::run(&controls, &args.program, &args.arguments);
    }

    let mut command = if let Some(profile) = args.seatbelt {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.args(["-p", &profile, &args.program]);
        command
    } else {
        Command::new(&args.program)
    };
    let status = command
        .env_clear()
        .args(&args.arguments)
        .status()
        .map_err(|error| format!("sandbox target failed to start: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use super::WindowsControls;
    use std::{
        mem::size_of,
        path::{Path, PathBuf},
        process::Command,
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, LocalFree, HANDLE, WAIT_OBJECT_0},
        Security::{
            Authorization::ConvertStringSidToSidW, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE,
            SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
            WRITE_RESTRICTED,
        },
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
                JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            },
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
                ResumeThread, WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_INFORMATION,
                STARTF_USESTDHANDLES, STARTUPINFOW,
            },
        },
    };

    struct Handles(Vec<HANDLE>);

    impl Drop for Handles {
        fn drop(&mut self) {
            for handle in self.0.drain(..) {
                if !handle.is_null() {
                    unsafe { CloseHandle(handle) };
                }
            }
        }
    }

    pub fn run(
        controls: &WindowsControls,
        program: &str,
        arguments: &[String],
    ) -> Result<i32, String> {
        validate_controls(controls)?;
        if !controls.network_allowed {
            return Err("Windows network denial is unavailable".into());
        }
        let workspace = Path::new(&controls.workspace_acl)
            .canonicalize()
            .map_err(|error| format!("workspace ACL root is invalid: {error}"))?;
        let writable = canonical_writable(&controls.writable_paths, &workspace)?;
        let sid = "S-1-5-12";
        let mut acl = AclLease {
            sid: sid.to_owned(),
            granted: Vec::new(),
            protected: Vec::new(),
        };
        for path in writable {
            icacls(&path, "/grant:r", &format!("*{sid}:(OI)(CI)M"))?;
            acl.granted.push(path);
        }
        if let Some(metadata) = protect_git_metadata(&workspace, &acl.granted, sid)? {
            acl.protected.push(metadata);
        }
        let result = unsafe {
            spawn(
                controls,
                program,
                arguments,
                Path::new(&controls.workspace_acl),
                sid,
            )
        };
        acl.cleanup()?;
        result
    }

    fn validate_controls(controls: &WindowsControls) -> Result<(), String> {
        if !controls.restricted_token
            || !controls.job.kill_on_close
            || controls.job.memory_bytes == 0
            || controls.job.max_pids == 0
            || controls.job.cpu_percent == 0
            || controls.job.cpu_percent > 10_000
        {
            return Err("invalid Windows sandbox controls".into());
        }
        Ok(())
    }

    fn canonical_writable(paths: &[String], workspace: &Path) -> Result<Vec<PathBuf>, String> {
        paths
            .iter()
            .map(|path| {
                let path = Path::new(path)
                    .canonicalize()
                    .map_err(|error| format!("writable ACL path is invalid: {error}"))?;
                if !path.starts_with(workspace) {
                    return Err("writable ACL path escapes workspace".into());
                }
                Ok(path)
            })
            .collect()
    }

    fn protect_git_metadata(
        workspace: &Path,
        writable: &[PathBuf],
        sid: &str,
    ) -> Result<Option<PathBuf>, String> {
        let metadata = workspace.join(".git");
        if !writable.iter().any(|path| metadata.starts_with(path)) {
            return Ok(None);
        }
        let kind = std::fs::symlink_metadata(&metadata)
            .map_err(|error| format!("Git worktree metadata cannot be protected: {error}"))?;
        if kind.file_type().is_symlink() {
            return Err("Git worktree metadata is a reparse point".into());
        }
        let permissions = if kind.is_dir() { "(OI)(CI)F" } else { "F" };
        icacls(&metadata, "/deny", &format!("*{sid}:{permissions}"))?;
        Ok(Some(metadata))
    }

    struct AclLease {
        sid: String,
        granted: Vec<PathBuf>,
        protected: Vec<PathBuf>,
    }

    impl AclLease {
        fn cleanup(&mut self) -> Result<(), String> {
            let mut errors = Vec::new();
            for path in self.protected.drain(..) {
                if let Err(error) = icacls(&path, "/remove:d", &format!("*{}*", self.sid)) {
                    errors.push(error);
                }
            }
            for path in self.granted.drain(..) {
                if let Err(error) = icacls(&path, "/remove:g", &format!("*{}*", self.sid)) {
                    errors.push(error);
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(format!("child ACL cleanup failed: {}", errors.join("; ")))
            }
        }
    }

    impl Drop for AclLease {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    fn icacls(path: &Path, operation: &str, principal: &str) -> Result<(), String> {
        let status = Command::new("icacls.exe")
            .arg(path)
            .args([operation, principal, "/Q"])
            .status()
            .map_err(|error| format!("icacls failed to start: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("icacls exited with {status}"))
        }
    }

    unsafe fn spawn(
        controls: &WindowsControls,
        program: &str,
        arguments: &[String],
        workspace: &Path,
        sid_text: &str,
    ) -> Result<i32, String> {
        let mut handles = Handles(Vec::new());
        let mut source_token = ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut source_token,
        ) == 0
        {
            return Err(last_error("OpenProcessToken"));
        }
        handles.0.push(source_token);

        let mut sid = ptr::null_mut();
        let sid_wide = wide(sid_text);
        if ConvertStringSidToSidW(sid_wide.as_ptr(), &mut sid) == 0 {
            return Err(last_error("ConvertStringSidToSidW"));
        }
        let restricted_sid = SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        };
        let mut token = ptr::null_mut();
        if CreateRestrictedToken(
            source_token,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &restricted_sid,
            &mut token,
        ) == 0
        {
            LocalFree(sid);
            return Err(last_error("CreateRestrictedToken"));
        }
        LocalFree(sid);
        handles.0.push(token);

        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        handles.0.push(job);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = controls.job.max_pids;
        limits.ProcessMemoryLimit = usize::try_from(controls.job.memory_bytes)
            .map_err(|_| "memory limit exceeds this Windows architecture".to_owned())?;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
        ) == 0
        {
            return Err(last_error("SetInformationJobObject(limits)"));
        }
        let cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
                CpuRate: u32::from(controls.job.cpu_percent) * 100,
            },
        };
        if SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            (&raw const cpu).cast(),
            u32::try_from(size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>()).unwrap(),
        ) == 0
        {
            return Err(last_error("SetInformationJobObject(CPU)"));
        }

        let mut command_line = wide(&windows_command_line(program, arguments));
        let current_directory = wide(&workspace.display().to_string());
        let startup = STARTUPINFOW {
            cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap(),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: GetStdHandle(STD_INPUT_HANDLE),
            hStdOutput: GetStdHandle(STD_OUTPUT_HANDLE),
            hStdError: GetStdHandle(STD_ERROR_HANDLE),
            ..STARTUPINFOW::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        if CreateProcessAsUserW(
            token,
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED,
            ptr::null(),
            current_directory.as_ptr(),
            &raw const startup,
            &raw mut process,
        ) == 0
        {
            return Err(last_error("CreateProcessAsUserW"));
        }
        handles.0.extend([process.hProcess, process.hThread]);
        if AssignProcessToJobObject(job, process.hProcess) == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        if ResumeThread(process.hThread) == u32::MAX {
            return Err(last_error("ResumeThread"));
        }
        if WaitForSingleObject(process.hProcess, INFINITE) != WAIT_OBJECT_0 {
            return Err(last_error("WaitForSingleObject"));
        }
        let mut exit_code = 1;
        if GetExitCodeProcess(process.hProcess, &mut exit_code) == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        Ok(i32::try_from(exit_code).unwrap_or(1))
    }

    fn last_error(operation: &str) -> String {
        format!("{operation} failed with Windows error {}", unsafe {
            GetLastError()
        })
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn windows_command_line(program: &str, arguments: &[String]) -> String {
        if program.to_ascii_lowercase().ends_with("cmd.exe") {
            if let Some(command) = arguments
                .iter()
                .position(|argument| argument.eq_ignore_ascii_case("/c"))
            {
                let switches = arguments[..=command].join(" ");
                let script = arguments[command + 1..].join(" ");
                return format!("{} {switches} {script}", quote(program));
            }
        }
        std::iter::once(program)
            .chain(arguments.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn quote(value: &str) -> String {
        let mut quoted = String::from("\"");
        let mut slashes = 0;
        for character in value.chars() {
            if character == '\\' {
                slashes += 1;
            } else {
                if character == '"' {
                    quoted.push_str(&"\\".repeat(slashes + 1));
                } else {
                    quoted.push_str(&"\\".repeat(slashes));
                }
                slashes = 0;
                quoted.push(character);
            }
        }
        quoted.push_str(&"\\".repeat(slashes * 2));
        quoted.push('"');
        quoted
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::LinuxControls;
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };
    use seccompiler::{apply_filter, BpfProgram, SeccompAction, SeccompFilter, SeccompRule};
    use std::{collections::BTreeMap, convert::TryInto, path::Path};

    pub fn restrict(controls: &LinuxControls) -> Result<(), String> {
        let workspace = Path::new(&controls.workspace)
            .canonicalize()
            .map_err(|error| format!("Linux workspace is invalid: {error}"))?;
        let writable = controls
            .writable
            .iter()
            .map(|path| {
                if path == "/tmp" {
                    return Ok(Path::new("/tmp").to_path_buf());
                }
                let path = Path::new(path)
                    .canonicalize()
                    .map_err(|error| format!("Linux writable path is invalid: {error}"))?;
                if !path.starts_with(&workspace) {
                    return Err("Linux writable path escapes workspace".to_owned());
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let abi = ABI::V1;
        let all = AccessFs::from_all(abi);
        let mut ruleset = Ruleset::default()
            .handle_access(all)
            .map_err(|error| error.to_string())?
            .create()
            .map_err(|error| error.to_string())?
            .set_compatibility(CompatLevel::HardRequirement)
            .add_rule(PathBeneath::new(
                PathFd::new("/").map_err(|error| error.to_string())?,
                AccessFs::from_read(abi) | AccessFs::Execute,
            ))
            .map_err(|error| error.to_string())?;
        for path in &writable {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    PathFd::new(path).map_err(|error| error.to_string())?,
                    all,
                ))
                .map_err(|error| error.to_string())?;
        }
        let status = ruleset.restrict_self().map_err(|error| error.to_string())?;
        if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
            return Err(format!("Landlock was not fully enforced: {status:?}"));
        }

        let mut denied: BTreeMap<i64, Vec<SeccompRule>> = [
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_ptrace,
            libc::SYS_bpf,
            libc::SYS_keyctl,
            libc::SYS_unshare,
            libc::SYS_setns,
        ]
        .into_iter()
        .map(|syscall| (syscall, Vec::new()))
        .collect();
        if !controls.network_allowed {
            for syscall in [
                libc::SYS_socket,
                libc::SYS_connect,
                libc::SYS_bind,
                libc::SYS_listen,
                libc::SYS_accept,
                libc::SYS_accept4,
                libc::SYS_sendto,
            ] {
                denied.insert(syscall, Vec::new());
            }
        }
        if controls.max_pids == 1 {
            for syscall in [libc::SYS_clone, libc::SYS_clone3] {
                denied.insert(syscall, Vec::new());
            }
            // fork and vfork are denied only where the kernel offers them.
            // aarch64 creates every process through clone, defines no such
            // syscall numbers, and libc therefore declares no constants to
            // name here -- so the pair is unreachable there rather than
            // unguarded.
            #[cfg(target_arch = "x86_64")]
            for syscall in [libc::SYS_fork, libc::SYS_vfork] {
                denied.insert(syscall, Vec::new());
            }
        }
        let filter: BpfProgram = SeccompFilter::new(
            denied,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            std::env::consts::ARCH
                .try_into()
                .map_err(|error: seccompiler::BackendError| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
        .try_into()
        .map_err(|error: seccompiler::BackendError| error.to_string())?;
        apply_filter(&filter).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_rejects_missing_limits_and_preserves_argv() {
        assert!(parse(vec!["--".into(), "echo".into()]).is_err());
        let limits =
            r#"{"memory_bytes":1024,"cpu_percent":100,"max_pids":2,"max_output_bytes":50}"#;
        let parsed = parse(vec![
            "--limits".into(),
            limits.into(),
            "--".into(),
            "echo".into(),
            "a b".into(),
        ])
        .unwrap();
        assert_eq!(parsed.program, "echo");
        assert_eq!(parsed.arguments, vec!["a b"]);
    }
}
