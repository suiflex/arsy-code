#[cfg(unix)]
use std::fs;
use std::process::Command;

#[cfg(unix)]
fn limits() -> serde_json::Value {
    serde_json::json!({
        "memory_bytes": 8 * 1024 * 1024 * 1024_u64,
        "cpu_percent": 100,
        "max_pids": 16,
        "max_output_bytes": 1024 * 1024,
    })
}

#[cfg(target_os = "macos")]
#[test]
fn native_seatbelt_denies_writes_outside_the_granted_workspace() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap().keep();
    symlink(&outside, workspace_path.join("escape-link")).unwrap();
    let profile = format!(
        "(version 1)\n(deny default)\n(allow process-exec process-fork signal)\n(allow file-read* (subpath \"/\"))\n(allow file-write* (subpath \"{}\"))\n(deny network*)\n",
        workspace_path.display()
    );
    let status = worker()
        .args([
            "--limits",
            &limits().to_string(),
            "--seatbelt",
            &profile,
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "printf ok > '{}'; printf escape > '{}/linked'; printf escape > '{}'",
                workspace_path.join("inside").display(),
                workspace_path.join("escape-link").display(),
                outside.join("escaped").display()
            ),
        ])
        .status()
        .unwrap();

    assert!(!status.success());
    assert_eq!(
        fs::read_to_string(workspace_path.join("inside")).unwrap(),
        "ok"
    );
    assert!(!outside.join("linked").exists());
    assert!(!outside.join("escaped").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn native_read_only_view_cannot_write_its_parent_workspace() {
    let repository = tempfile::tempdir().unwrap();
    let view = repository.path().join(".arsy/state/views/reader");
    fs::create_dir_all(&view).unwrap();
    let canonical = repository.path().join("canonical.txt");
    fs::write(&canonical, "original").unwrap();
    let profile = "(version 1)\n(deny default)\n(allow process-exec process-fork signal)\n(allow file-read* (subpath \"/\"))\n(deny network*)\n";
    let status = worker()
        .args([
            "--limits",
            &limits().to_string(),
            "--seatbelt",
            profile,
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "printf changed > '{}'; printf changed > '{}'",
                view.join("inside").display(),
                canonical.display()
            ),
        ])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!view.join("inside").exists());
    assert_eq!(fs::read_to_string(canonical).unwrap(), "original");
}

#[cfg(target_os = "macos")]
#[test]
fn native_seatbelt_denies_network_creation() {
    use std::net::TcpListener;

    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let profile = format!(
        "(version 1)\n(deny default)\n(allow process-exec process-fork signal)\n(allow file-read* (subpath \"/\"))\n(allow file-write* (subpath \"{}\"))\n",
        workspace.path().canonicalize().unwrap().display()
    );
    let status = worker()
        .args([
            "--limits",
            &limits().to_string(),
            "--seatbelt",
            &profile,
            "--",
            "/usr/bin/nc",
            "-z",
            "127.0.0.1",
            &port,
        ])
        .status()
        .unwrap();
    assert!(!status.success());
}

#[cfg(target_os = "linux")]
#[test]
fn native_landlock_and_seccomp_deny_file_and_network_escape() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap().keep();
    symlink(&outside, workspace.path().join("escape-link")).unwrap();
    let controls = serde_json::json!({
        "limits": limits(),
        "workspace": workspace.path(),
        "writable": [workspace.path()],
        "network_allowed": false,
    });
    let allowed = worker()
        .args([
            "--linux-controls",
            &controls.to_string(),
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "printf ok > '{}'",
                workspace.path().join("inside").display()
            ),
        ])
        .status()
        .unwrap();
    assert!(allowed.success(), "granted write failed with {allowed}");

    let denied = worker()
        .args([
            "--linux-controls",
            &controls.to_string(),
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "printf escape > '{}/linked'; printf escape > '{}'",
                workspace.path().join("escape-link").display(),
                outside.join("escaped").display()
            ),
        ])
        .status()
        .unwrap();

    assert!(!denied.success());
    assert_eq!(
        fs::read_to_string(workspace.path().join("inside")).unwrap(),
        "ok"
    );
    assert!(!outside.join("linked").exists());
    assert!(!outside.join("escaped").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn native_seccomp_denies_network_and_process_ceiling_blocks_forks() {
    let workspace = tempfile::tempdir().unwrap();
    let mut bounded = limits();
    bounded["max_pids"] = 1.into();
    let controls = serde_json::json!({
        "limits": bounded,
        "workspace": workspace.path(),
        "writable": [workspace.path()],
        "network_allowed": false,
    });
    for script in [
        "/usr/bin/python3 -c 'import socket; socket.socket()'",
        "/bin/sh -c true & wait",
    ] {
        let status = worker()
            .args([
                "--linux-controls",
                &controls.to_string(),
                "--",
                "/bin/sh",
                "-c",
                script,
            ])
            .status()
            .unwrap();
        assert!(!status.success(), "escape unexpectedly succeeded: {script}");
    }
}

#[cfg(unix)]
#[test]
fn native_worker_bounds_file_output() {
    let workspace = tempfile::tempdir().unwrap();
    let output = workspace.path().join("output");
    let mut bounded = limits();
    bounded["max_output_bytes"] = 1024.into();
    let status = worker()
        .args([
            "--limits",
            &bounded.to_string(),
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "/bin/dd if=/dev/zero of='{}' bs=2048 count=1",
                output.display()
            ),
        ])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(fs::metadata(output).unwrap().len() <= 1024);
}

#[cfg(windows)]
#[test]
fn native_restricted_token_job_and_acl_deny_writes_outside_the_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap().keep();
    let controls = serde_json::json!({
        "restricted_token": true,
        "job": {
            "memory_bytes": 512 * 1024 * 1024_u64,
            "cpu_percent": 100,
            "max_pids": 1,
            "kill_on_close": true,
        },
        "workspace_acl": workspace.path(),
        "writable_paths": [workspace.path()],
        "network_allowed": true,
    });
    let script = format!(
        "echo ok>inside & echo escape>{} & start /wait cmd.exe /D /C exit 0",
        outside.join("escaped").display()
    );
    let status = worker()
        .args([
            "--windows-controls",
            &controls.to_string(),
            "--",
            "cmd.exe",
            "/D",
            "/S",
            "/C",
            &script,
        ])
        .status()
        .unwrap();

    assert!(!status.success());
    assert!(workspace.path().join("inside").exists());
    assert!(!outside.join("escaped").exists());
}

fn worker() -> Command {
    Command::new(env!("CARGO_BIN_EXE_arsy-sandbox-worker"))
}
