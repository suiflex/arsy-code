//! Self-update implementation for ARSY CODE.
//!
//! Checks for the latest release from GitHub (`suiflex/arsy-code`), downloads
//! the binary archive matching the current platform and architecture, verifies
//! the SHA-256 digest, replaces `arsy` and `fluxguard`, and renders the ARSY
//! ASCII mark upon completion.

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

/// Render the ARSY logo banner to stdout.
pub fn print_logo(colour: bool) {
    let reset = if colour { "\x1b[0m" } else { "" };
    let green = if colour { "\x1b[38;2;53;208;127m" } else { "" };
    let cyan = if colour { "\x1b[38;2;53;200;255m" } else { "" };
    let bold = if colour { "\x1b[1m" } else { "" };
    let dim = if colour { "\x1b[2m" } else { "" };

    println!();
    for (i, row) in LOGO_MARK.iter().enumerate() {
        let text_part = match i {
            2 => format!("  {bold}{cyan}ARSY CODE{reset}"),
            3 => format!("  {dim}Auditable, model-independent agent harness{reset}"),
            5 => format!("  {green}Ready.{reset}"),
            _ => String::new(),
        };
        println!("{cyan}{row}{reset}{text_part}");
    }
    println!();
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
    let ext = if platform == "windows" { "zip" } else { "tar.gz" };
    Ok((platform, architecture, ext))
}

/// Fetch latest version tag from GitHub Releases.
pub fn fetch_latest_version(repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let headers = vec![
        ("accept".to_owned(), "application/vnd.github+json".to_owned()),
        (
            "user-agent".to_owned(),
            format!("arsy/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    let response = transport.get(url, headers).ok()?;
    if response.status != 200 {
        return None;
    }
    let body = response.lines.collect::<Result<Vec<_>, _>>().ok()?.join("\n");
    let val: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = val.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_owned())
}

/// Verify SHA-256 checksum of raw file bytes against expected checksum file text.
pub fn verify_sha256(bytes: &[u8], expected_content: &str) -> bool {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let actual = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let expected = expected_content
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    actual == expected
}

/// Download a URL to a file path, showing progress if curl is available.
fn download_file(url: &str, destination: &Path) -> Result<(), Diagnostic> {
    // If curl is installed, use curl with progress bar for best CLI UX
    if Command::new("curl").arg("--version").output().is_ok() {
        let status = Command::new("curl")
            .args([
                "-#",
                "-fL",
                "--retry",
                "3",
                "--proto",
                "=https",
                "--tlsv1.2",
                url,
                "-o",
            ])
            .arg(destination)
            .status()
            .map_err(|e| {
                Diagnostic::error(
                    "ARSY-UPD-1003",
                    format!("curl download failed: {e}"),
                    "ensure curl is installed and internet connection is active",
                )
            })?;
        if !status.success() {
            return Err(Diagnostic::error(
                "ARSY-UPD-1003",
                format!("failed to download from {url}"),
                "curl exited with non-zero status",
            ));
        }
        return Ok(());
    }

    // Fallback: download via ureq
    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let headers = vec![(
        "user-agent".to_owned(),
        format!("arsy/{}", env!("CARGO_PKG_VERSION")),
    )];
    let response = transport.get(url, headers).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1003",
            format!("download failed: {e}"),
            "check your internet connection",
        )
    })?;
    if response.status != 200 {
        return Err(Diagnostic::error(
            "ARSY-UPD-1003",
            format!("download returned HTTP {}", response.status),
            format!("failed to fetch {url}"),
        ));
    }
    let body = response
        .lines
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Diagnostic::error("ARSY-UPD-1003", e, "stream error"))?
        .join("\n");
    std::fs::write(destination, body.as_bytes()).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1003",
            format!("failed to write destination file: {e}"),
            "check directory permissions",
        )
    })?;
    Ok(())
}

/// Execute the `arsy update` subcommand.
pub fn execute_update(
    check_only: bool,
    force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("ARSY_REPOSITORY").unwrap_or_else(|_| REPO.to_owned());
    let (platform, architecture, ext) = detect_target()?;
    let archive_name = format!("arsy-{platform}-{architecture}.{ext}");

    let latest = fetch_latest_version(&repo).unwrap_or_else(|| current.to_owned());
    let up_to_date = latest == current;

    if check_only {
        let report = json!({
            "current_version": current,
            "latest_version": latest,
            "up_to_date": up_to_date,
            "check_only": true,
            "platform": platform,
            "architecture": architecture,
            "message": if up_to_date {
                format!("arsy-code v{current} is up to date.")
            } else {
                format!("update available: v{current} -> v{latest}")
            },
        });
        emitter.result(report);
        return Ok(0);
    }

    if up_to_date && !force {
        println!("arsy-code v{current} is up to date.");
        let report = json!({
            "current_version": current,
            "latest_version": latest,
            "up_to_date": true,
            "message": format!("arsy-code v{current} is up to date."),
        });
        emitter.result(report);
        return Ok(0);
    }

    let download_tag = if latest == current {
        format!("v{current}")
    } else {
        format!("v{latest}")
    };

    println!("==> Updating ARSY CODE: v{current} -> {download_tag}");

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

    let archive_path = temp_dir.path().join(&archive_name);
    let checksum_path = temp_dir.path().join(format!("{archive_name}.sha256"));

    let archive_url = format!("{download_base}/{archive_name}");
    let checksum_url = format!("{download_base}/{archive_name}.sha256");

    println!("==> Downloading {archive_name}...");
    download_file(&archive_url, &archive_path)?;
    download_file(&checksum_url, &checksum_path)?;

    println!("==> Verifying SHA-256 checksum...");
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

    println!("==> Extracting binaries...");
    let extract_status = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(temp_dir.path())
        .status()
        .map_err(|e| {
            Diagnostic::error(
                "ARSY-UPD-1007",
                format!("failed to extract archive with tar: {e}"),
                "ensure tar is available",
            )
        })?;
    if !extract_status.success() {
        return Err(Diagnostic::error(
            "ARSY-UPD-1007",
            "tar extraction returned non-zero status",
            "corrupted archive",
        ));
    }

    // Determine target installation directory
    let install_dir = if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            parent.to_path_buf()
        } else {
            default_install_dir()?
        }
    } else {
        default_install_dir()?
    };

    println!("==> Installing binaries to {}...", install_dir.display());
    install_binary(&temp_dir.path().join("arsy"), &install_dir.join("arsy"))?;
    if temp_dir.path().join("fluxguard").exists() {
        install_binary(
            &temp_dir.path().join("fluxguard"),
            &install_dir.join("fluxguard"),
        )?;
    }

    print_logo(true);
    println!(
        "ARSY CODE updated successfully: v{current} -> {download_tag} ({})",
        install_dir.join("arsy").display()
    );
    println!("Restart your terminal, then run:");
    println!("  arsy doctor");

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
    let home = std::env::var("HOME").map_err(|_| {
        Diagnostic::error(
            "ARSY-UPD-1008",
            "HOME environment variable not set",
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
        return Ok(());
    }
    // On Unix, write to temporary file beside destination and rename atomically
    let tmp_dest = destination.with_extension("tmp");
    std::fs::copy(source, &tmp_dest).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1009",
            format!("failed to write binary {}: {e}", tmp_dest.display()),
            "ensure write permissions on target directory",
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_dest, std::fs::Permissions::from_mode(0o755));
    }

    std::fs::rename(&tmp_dest, destination).map_err(|e| {
        Diagnostic::error(
            "ARSY-UPD-1009",
            format!("failed to replace binary {}: {e}", destination.display()),
            "check file locks or permissions",
        )
    })?;
    Ok(())
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
        let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let expected_line = format!("{hex}  arsy-test.tar.gz\n");
        assert!(verify_sha256(data, &expected_line));
        assert!(!verify_sha256(b"corrupted", &expected_line));
    }
}
