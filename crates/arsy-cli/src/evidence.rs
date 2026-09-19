//! `arsy artifact` and `arsy gc`: read stored evidence, and reclaim what no
//! event still points at.
//!
//! Nothing here prints raw stored bytes: an artifact reaches a terminal or a
//! file through the same redaction pipeline a model response does, because a
//! command output captured in a transcript is as much a sink as a network
//! request is.

use crate::{storage_failed, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_kernel::{
    artifact::{
        ArtifactMetadata, ArtifactReadLimits, ArtifactStore, FileArtifactStore, GcPlan, Sensitivity,
    },
    domain::ArtifactId,
    event::{EventEnvelope, EventPayload},
    service::AgentService,
};
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path};

/// Artifacts live beside the session store they are evidence for.
const ARTIFACT_PATH: &str = ".arsy/artifacts";

/// Bytes `artifact show` renders when the caller does not set `--max-bytes`.
/// Large enough for a command transcript, small enough not to flood a terminal.
const DEFAULT_SHOW_BYTES: u64 = 64 * 1024;

/// Ceiling on `artifact export`, which writes to a file rather than a screen.
const MAX_EXPORT_BYTES: u64 = 64 * 1024 * 1024;

/// A compressed artifact that expands beyond this multiple of its stored size
/// is refused rather than decoded: that shape is a decompression bomb.
const MAX_EXPANSION_RATIO: u64 = 100;

/// Default `--retention`: how long an artifact nothing references is kept
/// before `gc --apply` may remove it.
const DEFAULT_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

pub fn parse_artifact(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.is_empty() {
        return Err(usage(ARTIFACT_HELP));
    }
    let action = positional.remove(0);
    let reference = artifact_id(&crate::only_argument(
        positional,
        &format!("artifact {action}"),
        "<REF>",
    )?)?;
    match action.as_str() {
        "show" => Ok(Command::ArtifactShow {
            reference,
            max_bytes: arguments.max_bytes.unwrap_or(DEFAULT_SHOW_BYTES),
        }),
        "export" => Ok(Command::ArtifactExport {
            reference,
            out: arguments
                .out
                .clone()
                .ok_or_else(|| usage("artifact export requires --out <PATH>"))?,
        }),
        other => Err(usage(format!(
            "unknown artifact subcommand `{other}`\n{ARTIFACT_HELP}"
        ))),
    }
}

const ARTIFACT_HELP: &str =
    "artifact requires `show <REF> [--max-bytes N]` or `export <REF> --out <PATH>`";

pub fn parse_gc(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    if !arguments.positional.is_empty() {
        return Err(usage("gc takes no positional argument"));
    }
    Ok(Command::Gc {
        apply: arguments.apply,
        retention_ms: match &arguments.retention {
            Some(value) => duration_ms(value)?,
            None => DEFAULT_RETENTION_MS,
        },
    })
}

/// `artifact://<uuid>` or the bare UUID, which is what a JSON record carries.
fn artifact_id(value: &str) -> Result<ArtifactId, Diagnostic> {
    let bare = value
        .strip_prefix("artifact://")
        .unwrap_or_else(|| value.strip_prefix("artifact:").unwrap_or(value));
    bare.parse().map_err(|_| {
        usage(format!(
            "`{value}` is not an artifact reference; expected `artifact://<UUID>`"
        ))
    })
}

/// `30s`, `12h`, `7d`, or `2w`. Bare digits are seconds.
fn duration_ms(value: &str) -> Result<u64, Diagnostic> {
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1_000),
        Some(b'm') => (&value[..value.len() - 1], 60 * 1_000),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60 * 1_000),
        Some(b'd') => (&value[..value.len() - 1], 24 * 60 * 60 * 1_000),
        Some(b'w') => (&value[..value.len() - 1], 7 * 24 * 60 * 60 * 1_000),
        _ => (value, 1_000),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(multiplier))
        .ok_or_else(|| {
            usage(format!(
                "`{value}` is not a duration such as `30s`, `12h`, or `7d`"
            ))
        })
}

fn open(root: &Path, orphan_retention_ms: u64) -> Result<FileArtifactStore, Diagnostic> {
    FileArtifactStore::open(root.join(ARTIFACT_PATH), orphan_retention_ms)
        .map_err(|error| storage_failed(format!("{ARTIFACT_PATH}: {error}")))
}

/// `arsy artifact show`: a bounded, redacted excerpt plus the metadata that
/// says what the excerpt is part of.
pub fn show(
    invocation: &Invocation,
    reference: ArtifactId,
    max_bytes: u64,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = open(&root, DEFAULT_RETENTION_MS)?;
    let metadata = store
        .metadata(reference)
        .map_err(|error| not_found(reference, error))?;
    let redactor = crate::redactor(emitter)?;

    let bytes = store
        .read(
            reference,
            ArtifactReadLimits {
                max_bytes,
                max_expansion_ratio: MAX_EXPANSION_RATIO,
            },
        )
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-STL-1000",
                format!("artifact {reference} could not be read: {error}"),
                format!("raise the bound with `--max-bytes` (currently {max_bytes})"),
            )
        })?;
    let rendered = match std::str::from_utf8(&bytes) {
        Ok(text) => json!({
            "encoding": "utf-8",
            "content": redactor.sanitize(text).map_err(crate::secret_failed)?,
        }),
        // ponytail: binary evidence is described, not dumped. A hex window is
        // what a reader would want; add it when an operation stores one.
        Err(_) => json!({"encoding": "binary", "content": Value::Null}),
    };

    let report = json!({
        "artifact": reference.to_string(),
        "metadata": describe(&metadata),
        "bounded_to": max_bytes,
        "truncated": metadata.size > max_bytes,
        "rendered": rendered,
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"artifact": human_show(&report)})
    });
    Ok(0)
}

fn human_show(report: &Value) -> String {
    let metadata = &report["metadata"];
    let mut text = format!(
        "artifact {}\n  {} · {} bytes · {} · created by {}\n",
        report["artifact"].as_str().unwrap_or("?"),
        metadata["media_type"].as_str().unwrap_or("?"),
        metadata["size"],
        metadata["sensitivity"].as_str().unwrap_or("?"),
        metadata["creator"],
    );
    match report["rendered"]["content"].as_str() {
        Some(content) => {
            text.push_str("  ---\n");
            for line in content.lines() {
                text.push_str(&format!("  {line}\n"));
            }
        }
        None => text.push_str("  (binary content is not rendered)\n"),
    }
    if report["truncated"] == Value::Bool(true) {
        text.push_str(&format!(
            "  bounded to {} bytes; export it for the whole artifact\n",
            report["bounded_to"]
        ));
    }
    text
}

/// `arsy artifact export`: write one artifact where the caller asked, and say
/// what redaction changed on the way out.
pub fn export(
    invocation: &Invocation,
    reference: ArtifactId,
    out: &Path,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = open(&root, DEFAULT_RETENTION_MS)?;
    let metadata = store
        .metadata(reference)
        .map_err(|error| not_found(reference, error))?;
    let redactor = crate::redactor(emitter)?;
    let written = write_one(&store, &redactor, reference, out)?;
    emitter.result(json!({
        "artifact": reference.to_string(),
        "out": out.display().to_string(),
        "metadata": describe(&metadata),
        "bytes": written.bytes,
        "redaction": written.redaction,
    }));
    Ok(0)
}

struct Written {
    bytes: u64,
    redaction: Value,
}

fn write_one(
    store: &FileArtifactStore,
    redactor: &arsy_kernel::secret::Redactor,
    reference: ArtifactId,
    out: &Path,
) -> Result<Written, Diagnostic> {
    let bytes = store
        .read(
            reference,
            ArtifactReadLimits {
                max_bytes: MAX_EXPORT_BYTES,
                max_expansion_ratio: MAX_EXPANSION_RATIO,
            },
        )
        .map_err(|error| {
            storage_failed(format!("artifact {reference} could not be read: {error}"))
        })?;
    let (payload, redaction) = match std::str::from_utf8(&bytes) {
        Ok(text) => {
            let sanitized = redactor.sanitize(text).map_err(crate::secret_failed)?;
            let removed = sanitized.len() != text.len();
            (
                sanitized.into_bytes(),
                json!({
                    "scanned": true,
                    "rewritten": removed,
                    "handles_known": redactor.handles().count(),
                }),
            )
        }
        // A credential cannot be located inside bytes that are not text, so the
        // export says plainly that it was not scanned rather than implying it
        // was clean.
        Err(_) => (
            bytes,
            json!({"scanned": false, "reason": "artifact is not UTF-8 text"}),
        ),
    };
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(storage_failed)?;
    }
    std::fs::write(out, &payload)
        .map_err(|error| usage(format!("cannot write {}: {error}", out.display())))?;
    Ok(Written {
        bytes: payload.len() as u64,
        redaction,
    })
}

/// Write every artifact a session's events reference into `directory`.
/// Used by `arsy session export --include-artifacts`.
pub fn export_referenced(
    root: &Path,
    history: &[EventEnvelope],
    directory: &Path,
) -> Result<Value, Diagnostic> {
    let referenced = referenced_ids(history);
    if referenced.is_empty() {
        return Ok(json!({"exported": 0, "missing": []}));
    }
    let store = open(root, DEFAULT_RETENTION_MS)?;
    let redactor = arsy_kernel::secret::Redactor::new();
    std::fs::create_dir_all(directory).map_err(storage_failed)?;
    let mut exported = 0_u64;
    let mut missing = Vec::new();
    for id in referenced {
        if store.metadata(id).is_err() {
            missing.push(id.to_string());
            continue;
        }
        write_one(&store, &redactor, id, &directory.join(format!("{id}.bin")))?;
        exported += 1;
    }
    Ok(json!({
        "exported": exported,
        "directory": directory.display().to_string(),
        "missing": missing,
    }))
}

/// Artifact IDs any event in `history` points at.
fn referenced_ids(history: &[EventEnvelope]) -> Vec<ArtifactId> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    for event in history {
        if let EventPayload::Artifact { reference, .. } = &event.payload {
            if reference.scheme() != "artifact" {
                continue;
            }
            if let Ok(id) = reference.value().parse::<ArtifactId>() {
                if seen.insert(id) {
                    ordered.push(id);
                }
            }
        }
    }
    ordered
}

/// `arsy gc`: report what is unreachable and past retention. Deletion needs
/// `--apply`, so the default answer to "what would this remove" is never
/// "it already did".
pub fn collect(
    invocation: &Invocation,
    apply: bool,
    retention_ms: u64,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = open(&root, retention_ms)?;
    let reachable = reachable_artifacts(&root)?;
    let now = arsy_kernel::artifact::unix_time_ms();
    let plan = store
        .gc_plan(&reachable, now)
        .map_err(|error| storage_failed(error.to_string()))?;

    let mut report = json!({
        "applied": apply,
        "retention_ms": retention_ms,
        "reachable": reachable.len(),
        "unreachable_references": plan.references.len(),
        "orphan_objects": plan.objects.len(),
        "reclaimable_bytes": plan.reclaimed_bytes(),
        "references": plan
            .references
            .iter()
            .map(|metadata| json!({
                "artifact": metadata.id.to_string(),
                "media_type": metadata.media_type,
                "encoded_size": metadata.encoded_size,
                "sensitivity": sensitivity(metadata.sensitivity),
            }))
            .collect::<Vec<_>>(),
    });
    if apply {
        let removed = ArtifactStore::gc(&store, &reachable, now)
            .map_err(|error| storage_failed(error.to_string()))?;
        report["removed_references"] = json!(removed.references_removed);
        report["removed_objects"] = json!(removed.objects_removed);
    }
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"gc": human_gc(&report, &plan)})
    });
    Ok(0)
}

fn human_gc(report: &Value, plan: &GcPlan) -> String {
    if plan.is_empty() {
        return "Nothing is unreachable and past retention.".to_owned();
    }
    let mut text = format!(
        "{} unreachable reference(s) and {} orphan object(s), {} bytes\n",
        plan.references.len(),
        plan.objects.len(),
        plan.reclaimed_bytes(),
    );
    for metadata in &plan.references {
        text.push_str(&format!(
            "  {} · {} · {} bytes\n",
            metadata.id, metadata.media_type, metadata.encoded_size
        ));
    }
    text.push_str(if report["applied"] == Value::Bool(true) {
        "Removed."
    } else {
        "Nothing was removed; re-run with --apply."
    });
    text
}

/// Artifacts still pointed at by any event of any session in this workspace.
///
/// Reachability is defined by the event log because the log is the only
/// durable record that outlives a process: an artifact no event mentions can
/// never be produced as evidence again.
fn reachable_artifacts(root: &Path) -> Result<HashSet<ArtifactId>, Diagnostic> {
    let store = crate::open_store(root)?;
    let mut reachable = HashSet::new();
    for summary in store.sessions(usize::MAX).map_err(storage_failed)? {
        let history =
            AgentService::history(store.as_ref(), summary.session).map_err(storage_failed)?;
        reachable.extend(referenced_ids(&history));
    }
    Ok(reachable)
}

fn describe(metadata: &ArtifactMetadata) -> Value {
    json!({
        "media_type": metadata.media_type,
        "digest": metadata.digest.to_string(),
        "size": metadata.size,
        "encoded_size": metadata.encoded_size,
        "sensitivity": sensitivity(metadata.sensitivity),
        "creator": metadata.creator,
        "retain_until_ms": metadata.retain_until_ms,
    })
}

const fn sensitivity(value: Sensitivity) -> &'static str {
    match value {
        Sensitivity::Public => "public",
        Sensitivity::Internal => "internal",
        Sensitivity::Sensitive => "sensitive",
    }
}

fn not_found(reference: ArtifactId, error: arsy_kernel::artifact::ArtifactError) -> Diagnostic {
    Diagnostic::error(
        "ARSY-STL-1001",
        format!("artifact {reference} is not in this workspace: {error}"),
        "check the reference, or run `arsy session show <ID> --evidence` to list what a session recorded",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_the_documented_suffixes() {
        assert_eq!(duration_ms("30s").unwrap(), 30_000);
        assert_eq!(duration_ms("90").unwrap(), 90_000);
        assert_eq!(duration_ms("12h").unwrap(), 43_200_000);
        assert_eq!(duration_ms("7d").unwrap(), 604_800_000);
        assert_eq!(duration_ms("2w").unwrap(), 1_209_600_000);
        for invalid in ["", "d", "-1d", "1y", "18446744073709551615w"] {
            assert!(duration_ms(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn artifact_references_accept_both_written_forms() {
        let id = ArtifactId::new();
        assert_eq!(artifact_id(&id.to_string()).unwrap(), id);
        assert_eq!(artifact_id(&format!("artifact://{id}")).unwrap(), id);
        assert!(artifact_id("artifact://nope").is_err());
    }

    #[test]
    fn gc_defaults_to_a_dry_run_and_artifact_export_demands_a_destination() {
        let Command::Gc {
            apply,
            retention_ms,
        } = crate::parse(["gc".to_owned()]).unwrap().command
        else {
            panic!("gc parses to Gc");
        };
        assert!(!apply, "gc must never delete without --apply");
        assert_eq!(retention_ms, DEFAULT_RETENTION_MS);

        let id = ArtifactId::new().to_string();
        assert!(crate::parse(["artifact".to_owned(), "export".to_owned(), id.clone()]).is_err());
        assert!(crate::parse(["artifact".to_owned(), "show".to_owned(), id]).is_ok());
    }
}
