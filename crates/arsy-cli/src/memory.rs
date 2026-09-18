//! `arsy memory`: what this workspace remembers, and on whose authority.
//!
//! # Why a claim is an artifact
//!
//! A memory is evidence, so it is addressable like every other piece of
//! evidence: the record carries an artifact id, `arsy artifact show` renders
//! it, and `arsy gc` can see that a live record still needs it. Storing the
//! text inline in the record would have made memory the one kind of evidence
//! that could not be cited.
//!
//! # What the operator's authority buys
//!
//! Everything written here is a `user` record, because an operator typed it.
//! That is what lets it be believed above `Reported` and what lets it revoke
//! something a repository file put there. A claim the *agent* wants to keep
//! goes in at the authority of whatever wrote it, which is the rule the kernel
//! enforces rather than this command.

use crate::{usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_kernel::{
    artifact::{unix_time_ms, ArtifactReadLimits, ArtifactStore, NewArtifact, Sensitivity},
    capability::PolicySource,
    domain::MemoryId,
    memory::{Confidence, MemoryRecord, MemoryScope, MemoryStore, NewMemory},
};
use serde_json::{json, Value};
use std::path::Path;

/// How much of a claim is read back for display.
const CLAIM_LIMITS: ArtifactReadLimits = ArtifactReadLimits {
    max_bytes: 64 * 1024,
    max_expansion_ratio: 1_000,
};

/// Longest claim this command will store. A memory is a sentence, not a file:
/// anything longer belongs in the repository, where it can be read on demand.
const MAX_CLAIM_BYTES: usize = 4 * 1024;

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.is_empty() {
        return Err(usage(HELP));
    }
    let action = positional.remove(0);
    match action.as_str() {
        "list" => {
            if !positional.is_empty() {
                return Err(usage("memory list takes no positional argument"));
            }
            Ok(Command::MemoryList {
                scope: arguments.scope.clone(),
                all: arguments.all,
            })
        }
        "remember" => Ok(Command::MemoryRemember {
            claim: crate::only_argument(positional, "memory remember", "<CLAIM>")?,
            scope: arguments.scope.clone(),
        }),
        "forget" => {
            let id = crate::only_argument(positional, "memory forget", "<ID>")?;
            Ok(Command::MemoryForget {
                id: id
                    .parse()
                    .map_err(|_| usage(format!("`{id}` is not a memory record id")))?,
                reason: arguments
                    .to
                    .clone()
                    .unwrap_or_else(|| "withdrawn by the operator".to_owned()),
            })
        }
        other => Err(usage(format!("unknown memory action `{other}`\n{HELP}"))),
    }
}

const HELP: &str = "\
memory takes an action:
  arsy memory list [--scope <SCOPE>] [--all]   what this workspace remembers
  arsy memory remember <CLAIM> [--scope <SCOPE>]
  arsy memory forget <ID> [--to <REASON>]
Scopes: repository (default), user, working, heuristic, session:<ID>, task:<ID>, \
branch:<NAME>, team:<NAME>.";

pub fn list(
    invocation: &Invocation,
    scope: Option<String>,
    all: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let store = MemoryStore::open(crate::open_store(&root)?).map_err(crate::storage_failed)?;
    let artifacts = crate::artifact_store(&root)?;
    let now = unix_time_ms();

    let records: Vec<&MemoryRecord> = match &scope {
        Some(scope) => store.index().recall(&parse_scope(scope)?, now),
        None if all => store.index().all().collect(),
        None => {
            // Everything live, whatever scope it is in: the default question is
            // "what does this workspace believe", not "what is in one bucket".
            let mut live: Vec<&MemoryRecord> = store
                .index()
                .all()
                .filter(|record| record.is_live(now))
                .collect();
            live.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
            live
        }
    };

    let rendered: Vec<Value> = records
        .iter()
        .map(|record| render(record, &artifacts))
        .collect();
    emitter.result(if emitter.output == Output::Json {
        json!({"memories": rendered})
    } else {
        json!({"memory": human(&rendered)})
    });
    Ok(0)
}

pub fn remember(
    invocation: &Invocation,
    claim: &str,
    scope: Option<String>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let claim = claim.trim();
    if claim.is_empty() || claim.len() > MAX_CLAIM_BYTES {
        return Err(usage(format!(
            "a claim must be between 1 and {MAX_CLAIM_BYTES} bytes"
        )));
    }
    let root = crate::workspace_root(&invocation.workspace)?;
    let redactor = crate::redactor(emitter)?;
    let artifacts = crate::artifact_store(&root)?;
    let author = crate::actor();
    // Asked before the claim is written anywhere. `remember` applies the same
    // check, but it takes an artifact id -- so storing first and asking after
    // leaves a refused credential in the artifact store, which nothing
    // collects because a memory claim is kept by retention rather than by
    // reachability.
    arsy_kernel::memory::vet(claim, &redactor).map_err(refused)?;
    let stored = artifacts
        .put(
            claim.as_bytes(),
            NewArtifact {
                media_type: "text/plain".into(),
                creator: author.clone(),
                source_revision: None,
                sensitivity: Sensitivity::Internal,
                // A memory's claim is reachable for as long as the record is,
                // which is what `arsy gc` reads to decide it may not collect it.
                retain_until_ms: u64::MAX,
            },
        )
        .map_err(|error| crate::storage_failed(error.to_string()))?;

    let mut store = MemoryStore::open(crate::open_store(&root)?).map_err(crate::storage_failed)?;
    let id = store
        .remember(
            NewMemory {
                scope: parse_scope(scope.as_deref().unwrap_or("repository"))?,
                claim: stored.id,
                // The operator said it and nothing checked it. A claim with no
                // provenance is capped at `Reported` by the kernel, which is
                // exactly right for something typed at a prompt.
                provenance: Vec::new(),
                confidence: Confidence::Observed,
                origin: PolicySource::User,
                author,
                expires_at_ms: None,
                supersedes: None,
            },
            claim,
            &redactor,
            unix_time_ms(),
        )
        .map_err(refused)?;

    emitter.result(json!({
        "memory": id.to_string(),
        "claim": stored.id.to_string(),
    }));
    Ok(0)
}

pub fn forget(
    invocation: &Invocation,
    id: MemoryId,
    reason: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let mut store = MemoryStore::open(crate::open_store(&root)?).map_err(crate::storage_failed)?;
    store
        .revoke(id, PolicySource::User, reason, unix_time_ms())
        .map_err(refused)?;
    emitter.result(json!({"memory": id.to_string(), "revoked": reason}));
    Ok(0)
}

/// What the model is told this workspace believes, if anything.
///
/// Repository scope only: a user's own memory travels with the operator, not
/// with the checkout, and session or task memory belongs to a run that is over.
/// Bounded by count and by bytes, because a memory that crowds out the task is
/// worse than one that was never recalled.
pub fn recalled(root: &Path, max_bytes: usize) -> Option<String> {
    let store = MemoryStore::open(crate::open_store(root).ok()?).ok()?;
    let artifacts = crate::artifact_store(root).ok()?;
    let mut text = String::new();
    for record in store
        .index()
        .recall(&MemoryScope::Repository, unix_time_ms())
    {
        let Some(claim) = claim_text(record, &artifacts) else {
            continue;
        };
        let line = format!(
            "- {} (confidence: {})\n",
            claim.trim(),
            confidence(record.confidence)
        );
        if text.len() + line.len() > max_bytes {
            break;
        }
        text.push_str(&line);
    }
    (!text.is_empty()).then(|| {
        format!(
            "<workspace-memory>\nRecorded facts about this workspace:\n{text}</workspace-memory>"
        )
    })
}

fn render(record: &MemoryRecord, artifacts: &dyn ArtifactStore) -> Value {
    json!({
        "id": record.id.to_string(),
        "scope": record.scope.to_string(),
        "claim": claim_text(record, artifacts),
        "confidence": confidence(record.confidence),
        "origin": record.origin.to_string(),
        "status": serde_json::to_value(record.status).unwrap_or(Value::Null),
        "updated_at_ms": record.updated_at_ms,
        "revocation": record.revocation,
    })
}

fn claim_text(record: &MemoryRecord, artifacts: &dyn ArtifactStore) -> Option<String> {
    let bytes = artifacts.read(record.claim, CLAIM_LIMITS).ok()?;
    String::from_utf8(bytes).ok()
}

fn confidence(confidence: Confidence) -> String {
    serde_json::to_value(confidence)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "reported".to_owned())
}

fn human(records: &[Value]) -> String {
    if records.is_empty() {
        return "This workspace remembers nothing.".to_owned();
    }
    records
        .iter()
        .map(|record| {
            format!(
                "{} · {} · {}\n  {}\n",
                record["id"].as_str().unwrap_or_default(),
                record["scope"].as_str().unwrap_or_default(),
                record["confidence"].as_str().unwrap_or_default(),
                record["claim"].as_str().unwrap_or("<unreadable claim>"),
            )
        })
        .collect()
}

fn parse_scope(raw: &str) -> Result<MemoryScope, Diagnostic> {
    let (kind, value) = raw.split_once(':').unwrap_or((raw, ""));
    Ok(match (kind, value) {
        ("repository", "") => MemoryScope::Repository,
        ("user", "") => MemoryScope::User,
        ("working", "") => MemoryScope::Working,
        ("heuristic", "") => MemoryScope::Heuristic,
        ("session", id) if !id.is_empty() => MemoryScope::Session(id.to_owned()),
        ("task", id) if !id.is_empty() => MemoryScope::Task(id.to_owned()),
        ("branch", name) if !name.is_empty() => MemoryScope::Branch(name.to_owned()),
        ("team", name) if !name.is_empty() => MemoryScope::Team(name.to_owned()),
        _ => return Err(usage(format!("`{raw}` is not a memory scope\n{HELP}"))),
    })
}

fn refused(error: arsy_kernel::memory::MemoryError) -> Diagnostic {
    Diagnostic::error(
        "ARSY-POL-1001",
        error.to_string(),
        "check `arsy memory list --all`, or record the claim in a scope you may write",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_actions_and_scopes_are_validated_before_anything_is_stored() {
        assert!(crate::parse(vec!["memory".to_owned()]).is_err());
        assert!(crate::parse(vec!["memory".to_owned(), "guess".to_owned()]).is_err());
        assert!(crate::parse(vec!["memory".to_owned(), "remember".to_owned()]).is_err());

        assert_eq!(parse_scope("repository").unwrap(), MemoryScope::Repository);
        assert_eq!(
            parse_scope("branch:main").unwrap(),
            MemoryScope::Branch("main".to_owned())
        );
        // A scope that needs a name and has none is refused rather than
        // silently becoming a differently scoped record.
        assert!(parse_scope("branch").is_err());
        assert!(parse_scope("repository:x").is_err());
    }
}
