//! Self-update implementation for ARSY CODE.
//!
//! Checks for the latest release from GitHub (`suiflex/arsy-code`), downloads
//! the binary archive matching the current platform and architecture, verifies
//! the SHA-256 digest, extracts `arsy` and `fluxguard`, health-checks the new
//! binary, and provides automatic rollback on failure.

use crate::Diagnostic;
use crate::Emitter;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "suiflex/arsy-code";

/// ARSY banner mark rows.
const LOGO_MARK: [&str; 8] = [
    "         ++++++==         ",
    "       ***++++++===       ",
    "      ****      +===      ",
    "      ***        +==      ",
    "   +****   ****   ++===   ",
    "  **+**   +++***   +====  ",
    " ****+   ++++++++   +==== ",
    "*****    ==++++++    +====",
];

/// Render the ARSY logo banner to stdout in human mode.
pub fn print_logo(colour: bool) {
    let reset = if colour { "\x1b[0m" } else { "" };
    let green = if colour { "\x1b[38;2;53;208;127m" } else { "" };
    let cyan = if colour { "\x1b[38;2;53;200;255m" } else { "" };
    let bold = if colour { "\x1b[1m" } else { "" };
    let dim = if colour { "\x1b[2m" } else { "" };

    eprintln!();
    for (i, row) in LOGO_MARK.iter().enumerate() {
        let text_part = match i {
            2 => format!("  {bold}{cyan}ARSY CODE{reset}"),
            3 => format!("  {dim}Auditable, model-independent agent harness{reset}"),
            5 => format!("  {green}Ready.{reset}"),
            _ => String::new(),
        };
        eprintln!("{cyan}{row}{reset}{text_part}");
    }
    eprintln!();
}

/// Detect the platform and architecture strings matching GitHub Release assets.
pub fn detect_target() -> Result<(&'static str, &'static str, &'static str), Diagnostic> {
    let platform = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        other => {
            return Err(Diagnostic::error(
                "ARSY-UPD-1001",
                format!("unsupported operating system: {other}"),
                "self-update is supported on macOS, Linux, and Windows",
            ))
        }
    };
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(Diagnostic::error(
                "ARSY-UPD-1002",
                format!("unsupported architecture: {other}"),
                "self-update is supported on x86_64 and aarch64",
            ))
        }
    };
    let ext = if platform == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    Ok((platform, architecture, ext))
}

/// Fetch latest version tag from GitHub Releases with proper error reporting.
pub fn fetch_latest_version(repo: &str) -> Result<String, Diagnostic> {
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_')
    {
        return Err(Diagnostic::error(
            "ARSY-UPD-1000",
            format!("invalid repository specification: {repo}"),
            "repository must contain only alphanumeric characters, slashes, hyphens, and underscores",
        ));
    }

    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("arsy/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    let response = agent
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .call()
        .map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1000",
                format!("failed to connect to release server: {e}"),
                "verify network connectivity or check if GitHub is reachable",
            )
        })?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(Diagnostic::error(
            "ARSY-UPD-1000",
            format!("release query returned HTTP {status}"),
            if status == 403 {
                "GitHub API rate limit exceeded; try again later or verify network"
            } else {
                "check that the release exists on GitHub"
            },
        ));
    }

    let mut reader = response.into_body().into_reader();
    let val: serde_json::Value = serde_json::from_reader(&mut reader).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1000",
            format!("malformed release response: {e}"),
            "unexpected response format from release server",
        )
    })?;

    let tag = val
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Diagnostic::error(
                "ARSY-UPD-1000",
                "release missing `tag_name` field",
                "release metadata is incomplete",
            )
        })?;

    let clean = tag.trim_start_matches('v').trim();
    if clean.is_empty()
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(Diagnostic::error(
            "ARSY-UPD-1000",
            format!("invalid version tag: {tag}"),
            "version tag must contain valid semver characters",
        ));
    }
    Ok(clean.to_owned())
}

/// Verify SHA-256 checksum of raw file bytes against expected checksum file text.
pub fn verify_sha256(bytes: &[u8], expected_content: &str) -> bool {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let actual = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let expected = expected_content
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    actual == expected
}

/// Download a URL directly to a file destination safely preserving raw binary bytes.
fn download_file(url: &str, destination: &Path, is_human: bool) -> Result<(), Diagnostic> {
    if Command::new("curl").arg("--version").output().is_ok() {
        let mut cmd = Command::new("curl");
        cmd.args(["-fL", "--retry", "3", "--proto", "=https", "--tlsv1.2"]);
        if is_human {
            cmd.arg("-#");
        } else {
            cmd.args(["-sS"]);
        }
        cmd.arg(url).arg("-o").arg(destination);

        let status = cmd.status().map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1003",
                format!("curl execution failed: {e}"),
                "ensure curl is available or check network",
            )
        })?;
        if status.success() {
            return Ok(());
        }
    }

    // Binary-safe fallback using ureq streaming
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("arsy/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    let response = agent.get(url).call().map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1003",
            format!("download failed: {e}"),
            "check your internet connection",
        )
    })?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(Diagnostic::error(
            "ARSY-UPD-1003",
            format!("download returned HTTP {status}"),
            format!("failed to fetch {url}"),
        ));
    }

    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(destination).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1003",
            format!("failed to create destination file: {e}"),
            "check directory write permissions",
        )
    })?;

    std::io::copy(&mut reader, &mut file).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1003",
            format!("failed to stream download: {e}"),
            "download was interrupted",
        )
    })?;

    Ok(())
}

struct UpdateOptions<'a> {
    current: &'a str,
    repo: &'a str,
    platform: &'a str,
    architecture: &'a str,
    check_only: bool,
    force: bool,
    is_human: bool,
}

/// Check if an update is needed, reporting to emitter when check-only or up-to-date.
fn evaluate_version(
    options: &UpdateOptions<'_>,
    emitter: &mut Emitter,
) -> Result<Option<String>, Diagnostic> {
    let latest = fetch_latest_version(options.repo)?;
    let up_to_date = latest == options.current;

    if options.check_only {
        let report = json!({
            "current_version": options.current,
            "latest_version": latest,
            "up_to_date": up_to_date,
            "check_only": true,
            "platform": options.platform,
            "architecture": options.architecture,
            "message": if up_to_date {
                format!("arsy-code v{} is up to date.", options.current)
            } else {
                format!("update available: v{} -> v{latest}", options.current)
            },
        });
        emitter.result(report);
        return Ok(None);
    }

    if up_to_date && !options.force {
        if options.is_human {
            eprintln!("arsy-code v{} is already up to date.", options.current);
        }
        let report = json!({
            "current_version": options.current,
            "latest_version": latest,
            "up_to_date": true,
            "message": format!("arsy-code v{} is already up to date.", options.current),
        });
        emitter.result(report);
        return Ok(None);
    }

    let download_tag = if latest == options.current {
        format!("v{}", options.current)
    } else {
        format!("v{latest}")
    };
    Ok(Some(download_tag))
}

/// Check for active ARSY sessions before replacing binaries.
fn check_active_sessions(force: bool) -> Result<(), Diagnostic> {
    if force {
        return Ok(());
    }
    let current_pid = std::process::id();
    #[cfg(unix)]
    {
        if let Ok(output) = Command::new("pgrep").arg("-x").arg("arsy").output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let pids: Vec<u32> = stdout
                    .lines()
                    .filter_map(|l| l.trim().parse::<u32>().ok())
                    .filter(|pid| *pid != current_pid)
                    .collect();
                if !pids.is_empty() {
                    return Err(Diagnostic::error(
                        "ARSY-UPD-1011",
                        format!("active ARSY session detected (pid(s): {:?})", pids),
                        "stop active sessions before replacing binaries, or pass --force",
                    ));
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(output) = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq arsy.exe", "/NH"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let count = stdout.lines().filter(|l| l.contains("arsy.exe")).count();
            if count > 1 {
                return Err(Diagnostic::error(
                    "ARSY-UPD-1011",
                    "active ARSY processes detected",
                    "stop active sessions before replacing binaries, or pass --force",
                ));
            }
        }
    }
    Ok(())
}

/// Download release archive and verify SHA-256 digest.
fn download_and_verify(
    download_base: &str,
    archive_name: &str,
    temp_dir: &Path,
    is_human: bool,
) -> Result<PathBuf, Diagnostic> {
    let archive_path = temp_dir.join(archive_name);
    let checksum_path = temp_dir.join(format!("{archive_name}.sha256"));
    let archive_url = format!("{download_base}/{archive_name}");
    let checksum_url = format!("{download_base}/{archive_name}.sha256");

    if is_human {
        eprintln!("==> Downloading {archive_name}...");
    }
    download_file(&archive_url, &archive_path, is_human)?;
    download_file(&checksum_url, &checksum_path, false)?;

    if is_human {
        eprintln!("==> Verifying SHA-256 checksum...");
    }
    let archive_bytes = std::fs::read(&archive_path).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1005",
            format!("failed to read downloaded archive: {e}"),
            "check file permissions",
        )
    })?;
    let expected_checksum = std::fs::read_to_string(&checksum_path).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1005",
            format!("failed to read checksum file: {e}"),
            "check file permissions",
        )
    })?;

    if !verify_sha256(&archive_bytes, &expected_checksum) {
        return Err(Diagnostic::error(
            "ARSY-UPD-1006",
            "checksum verification failed",
            "the downloaded archive does not match the published SHA-256 digest",
        ));
    }
    Ok(archive_path)
}

/// Extract archive and validate presence of both required binaries.
fn extract_and_validate(
    archive_path: &Path,
    extract_dir: &Path,
    platform: &str,
    is_human: bool,
) -> Result<(PathBuf, PathBuf, &'static str, &'static str), Diagnostic> {
    if is_human {
        eprintln!("==> Extracting binaries...");
    }

    let extract_status = if platform == "windows" {
        let tar_attempt = Command::new("tar")
            .args(["-xf"])
            .arg(archive_path)
            .arg("-C")
            .arg(extract_dir)
            .status();
        match tar_attempt {
            Ok(status) if status.success() => Ok(status),
            _ => Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Expand-Archive",
                    "-Path",
                ])
                .arg(archive_path)
                .args(["-DestinationPath"])
                .arg(extract_dir)
                .arg("-Force")
                .status(),
        }
    } else {
        Command::new("tar")
            .args(["-xzf"])
            .arg(archive_path)
            .arg("-C")
            .arg(extract_dir)
            .status()
    };

    match extract_status {
        Ok(status) if status.success() => {}
        _ => {
            return Err(Diagnostic::error(
                "ARSY-UPD-1007",
                "archive extraction failed",
                "corrupted or unsupported archive format",
            ));
        }
    }

    let (arsy_name, fluxguard_name) = if platform == "windows" {
        ("arsy.exe", "fluxguard.exe")
    } else {
        ("arsy", "fluxguard")
    };

    let new_arsy = extract_dir.join(arsy_name);
    let new_fluxguard = extract_dir.join(fluxguard_name);

    if !new_arsy.is_file() {
        return Err(Diagnostic::error(
            "ARSY-UPD-1007",
            format!("release archive is missing required binary `{arsy_name}`"),
            "both arsy and fluxguard are required in the release archive",
        ));
    }
    if !new_fluxguard.is_file() {
        return Err(Diagnostic::error(
            "ARSY-UPD-1007",
            format!("release archive is missing required binary `{fluxguard_name}`"),
            "both arsy and fluxguard are required in the release archive",
        ));
    }

    Ok((new_arsy, new_fluxguard, arsy_name, fluxguard_name))
}

/// Backup targets and install new binaries with automatic rollback if health check fails.
fn install_with_rollback(
    new_arsy: &Path,
    new_fluxguard: &Path,
    install_dir: &Path,
    arsy_name: &str,
    fluxguard_name: &str,
    is_human: bool,
) -> Result<PathBuf, Diagnostic> {
    let target_arsy = install_dir.join(arsy_name);
    let target_fluxguard = install_dir.join(fluxguard_name);

    if is_human {
        eprintln!("==> Installing binaries to {}...", install_dir.display());
    }

    let backup_arsy = install_dir.join(format!("{arsy_name}.bak"));
    let backup_fluxguard = install_dir.join(format!("{fluxguard_name}.bak"));

    // Both backups exist before either binary is touched: a rollback that
    // finds no backup, or an older one, would restore the wrong version.
    let backed_up_arsy = back_up(&target_arsy, &backup_arsy)?;
    let backed_up_fluxguard = back_up(&target_fluxguard, &backup_fluxguard)?;

    let install_and_check = || -> Result<(), Diagnostic> {
        install_binary(new_arsy, &target_arsy)?;
        install_binary(new_fluxguard, &target_fluxguard)?;

        let check = Command::new(&target_arsy).arg("--version").output();
        match check {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(Diagnostic::error(
                "ARSY-UPD-1010",
                format!(
                    "installed binary health check failed (status: {:?})",
                    output.status.code()
                ),
                "rolling back to previous binary",
            )),
            Err(e) => Err(Diagnostic::error(
                "ARSY-UPD-1010",
                format!("failed to execute installed binary: {e}"),
                "rolling back to previous binary",
            )),
        }
    };

    if let Err(err) = install_and_check() {
        let failures: Vec<String> = [
            roll_back(&target_arsy, &backup_arsy, backed_up_arsy),
            roll_back(&target_fluxguard, &backup_fluxguard, backed_up_fluxguard),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect();
        if failures.is_empty() {
            return Err(err);
        }
        return Err(Diagnostic::error(
            "ARSY-UPD-1013",
            format!(
                "{}; rollback also failed: {}",
                err.message,
                failures.join("; ")
            ),
            format!(
                "restore the retained `.bak` binaries in {} by hand",
                install_dir.display()
            ),
        ));
    }

    Ok(target_arsy)
}

/// Copy an installed binary to its `.bak`, returning whether one was made.
///
/// A binary that is not installed has nothing to back up, and any `.bak`
/// already beside it is left alone rather than trusted: it belongs to an
/// earlier update, so [`roll_back`] never restores it.
fn back_up(target: &Path, backup: &Path) -> Result<bool, Diagnostic> {
    if !target.exists() {
        return Ok(false);
    }
    std::fs::copy(target, backup).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1012",
            format!("failed to back up {}: {e}", target.display()),
            "nothing was replaced; free disk space or fix permissions and retry",
        )
    })?;
    Ok(true)
}

/// Put back what [`back_up`] saved, or remove a binary this update placed
/// where none was installed before.
fn roll_back(target: &Path, backup: &Path, backed_up: bool) -> Result<(), String> {
    if backed_up {
        return install_binary(backup, target).map_err(|d| d.message);
    }
    match std::fs::remove_file(target) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("failed to remove {}: {e}", target.display()))
        }
        _ => Ok(()),
    }
}

/// Execute the `arsy update` subcommand with release verification and rollback guarantees.
pub fn execute_update(
    check_only: bool,
    force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("ARSY_REPOSITORY").unwrap_or_else(|_| REPO.to_owned());
    let (platform, architecture, ext) = detect_target()?;
    let is_human = emitter.output == crate::Output::Human;

    let options = UpdateOptions {
        current,
        repo: &repo,
        platform,
        architecture,
        check_only,
        force,
        is_human,
    };

    let download_tag = match evaluate_version(&options, emitter)? {
        Some(tag) => tag,
        None => return Ok(0),
    };

    check_active_sessions(force)?;

    if is_human {
        eprintln!("==> Updating ARSY CODE: v{current} -> {download_tag}");
    }

    let download_base = if let Ok(base) = std::env::var("ARSY_DOWNLOAD_BASE") {
        base.trim_end_matches('/').to_owned()
    } else {
        format!("https://github.com/{repo}/releases/download/{download_tag}")
    };

    let temp_dir = tempfile::Builder::new()
        .prefix("arsy-update-")
        .tempdir()
        .map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1004",
                format!("failed to create temporary directory: {e}"),
                "ensure system temp directory is writable",
            )
        })?;

    let archive_name = format!("arsy-{platform}-{architecture}.{ext}");
    let archive_path =
        download_and_verify(&download_base, &archive_name, temp_dir.path(), is_human)?;

    let (new_arsy, new_fluxguard, arsy_name, fluxguard_name) =
        extract_and_validate(&archive_path, temp_dir.path(), platform, is_human)?;

    let install_dir = match std::env::current_exe() {
        Ok(current_exe) if current_exe.parent().is_some() => {
            current_exe.parent().unwrap().to_path_buf()
        }
        _ => default_install_dir()?,
    };

    let target_arsy = install_with_rollback(
        &new_arsy,
        &new_fluxguard,
        &install_dir,
        arsy_name,
        fluxguard_name,
        is_human,
    )?;

    if is_human {
        print_logo(true);
        eprintln!(
            "ARSY CODE updated successfully: v{current} -> {download_tag} ({})",
            target_arsy.display()
        );
        eprintln!("Restart your terminal, then run:");
        eprintln!("  arsy doctor");
    }

    let report = json!({
        "current_version": current,
        "updated_version": download_tag,
        "install_dir": install_dir.to_string_lossy(),
        "status": "success"
    });
    emitter.result(report);
    Ok(0)
}

fn default_install_dir() -> Result<PathBuf, Diagnostic> {
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let dir = PathBuf::from(local_app_data).join("Programs").join("arsy");
            std::fs::create_dir_all(&dir).map_err(|e| {
                Diagnostic::error(
                    "ARSY-UPD-1008",
                    format!("failed to create install directory: {e}"),
                    "check permissions",
                )
            })?;
            return Ok(dir);
        }
        if let Ok(user_profile) = std::env::var("USERPROFILE") {
            let dir = PathBuf::from(user_profile).join(".local").join("bin");
            std::fs::create_dir_all(&dir).map_err(|e| {
                Diagnostic::error(
                    "ARSY-UPD-1008",
                    format!("failed to create install directory: {e}"),
                    "check permissions",
                )
            })?;
            return Ok(dir);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            Diagnostic::error(
                "ARSY-UPD-1008",
                "HOME or USERPROFILE environment variable not set",
                "set HOME or run with appropriate permissions",
            )
        })?;
    let dir = PathBuf::from(home).join(".local/bin");
    std::fs::create_dir_all(&dir).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1008",
            format!("failed to create install directory: {e}"),
            "check permissions",
        )
    })?;
    Ok(dir)
}

fn install_binary(source: &Path, destination: &Path) -> Result<(), Diagnostic> {
    if !source.exists() {
        return Err(Diagnostic::error(
            "ARSY-UPD-1009",
            format!("source binary `{}` does not exist", source.display()),
            "corrupted installation source",
        ));
    }

    // Windows running .exe replacement: rename running executable first, then copy
    #[cfg(windows)]
    {
        let tmp_old = destination.with_extension("exe.old");
        if destination.exists() {
            let _ = std::fs::remove_file(&tmp_old);
            let _ = std::fs::rename(destination, &tmp_old);
        }
        std::fs::copy(source, destination).map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1009",
                format!("failed to copy binary {}: {e}", destination.display()),
                "check permissions",
            )
        })?;
        return Ok(());
    }

    // Unix running executable replacement: copy to temporary file beside destination and atomic rename
    #[cfg(not(windows))]
    {
        let tmp_dest = destination.with_extension("tmp");
        std::fs::copy(source, &tmp_dest).map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1009",
                format!("failed to write binary {}: {e}", tmp_dest.display()),
                "ensure write permissions on target directory",
            )
        })?;

        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_dest, std::fs::Permissions::from_mode(0o755));

        std::fs::rename(&tmp_dest, destination).map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1009",
                format!("failed to replace binary {}: {e}", destination.display()),
                "check file locks or permissions",
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_target_succeeds() {
        let (platform, arch, ext) = detect_target().unwrap();
        assert!(!platform.is_empty());
        assert!(!arch.is_empty());
        assert!(!ext.is_empty());
    }

    #[test]
    fn test_verify_sha256() {
        let data = b"hello arsy code";
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        let digest = hasher.finalize();
        let hex = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let expected_line = format!("{hex}  arsy-test.tar.gz\n");
        assert!(verify_sha256(data, &expected_line));
        assert!(!verify_sha256(b"corrupted", &expected_line));
    }

    #[test]
    fn a_backup_that_cannot_be_made_stops_the_update() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("arsy");
        std::fs::write(&target, b"old").unwrap();
        // A directory where the backup file should go makes the copy fail.
        let backup = dir.path().join("arsy.bak");
        std::fs::create_dir(&backup).unwrap();
        let error = back_up(&target, &backup).unwrap_err();
        assert_eq!(error.code, "ARSY-UPD-1012");
        assert_eq!(std::fs::read(&target).unwrap(), b"old", "nothing replaced");
    }

    #[test]
    fn a_stale_backup_is_never_restored() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fluxguard");
        let backup = dir.path().join("fluxguard.bak");
        std::fs::write(&backup, b"stale").unwrap();
        let backed_up = back_up(&target, &backup).unwrap();
        assert!(
            !backed_up,
            "nothing was installed, so nothing was backed up"
        );

        std::fs::write(&target, b"new").unwrap();
        roll_back(&target, &backup, backed_up).unwrap();
        assert!(!target.exists(), "the binary this update placed is removed");
        assert_eq!(std::fs::read(&backup).unwrap(), b"stale", "left alone");
    }

    #[test]
    fn a_rollback_restores_the_backup_this_update_made() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("arsy");
        let backup = dir.path().join("arsy.bak");
        std::fs::write(&target, b"old").unwrap();
        assert!(back_up(&target, &backup).unwrap());
        std::fs::write(&target, b"broken").unwrap();
        roll_back(&target, &backup, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn a_failed_rollback_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("arsy");
        let missing = dir.path().join("arsy.bak");
        assert!(roll_back(&target, &missing, true).is_err());
    }
}
