//! `arsy policy explain`: answer "would this be allowed" without doing it.
//!
//! The command evaluates the same compiled rule set an execution would, over
//! the same risk context, and prints the deciding rule and the full trace. It
//! dispatches nothing: the only subprocess it runs is the read-only `git
//! status` that decides whether the working tree is dirty, because that input
//! changes the answer.

use crate::{load_session_config, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_kernel::{
    capability::{CapabilityAction, CapabilityRequirement},
    config::effect_name,
    domain::{OperationId, Principal, ResourceRef},
    operation::{Idempotency, OperationKind, OperationRequest},
    policy::{PolicyDecision, PolicyQuery, RiskContext, RuleEffect, WorkspaceCleanliness},
};
use serde_json::{json, Value};
use std::path::Path;

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.first().map(String::as_str) != Some("explain") {
        return Err(usage(
            "policy requires `explain <OPERATION> [--resource <REF>] [--actor <ID>]`",
        ));
    }
    positional.remove(0);
    Ok(Command::PolicyExplain {
        operation: crate::only_argument(positional, "policy explain", "<OPERATION>")?,
        resource: arguments.resource.clone(),
        actor: arguments.actor.clone(),
    })
}

pub fn explain(
    invocation: &Invocation,
    operation: &str,
    resource: Option<&str>,
    actor: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_session_config(&root, &working, invocation)?;

    let kind = OperationKind::new(operation)
        .map_err(|_| usage(format!("`{operation}` is not a canonical operation kind")))?;
    let workspace = arsy_code::resource::Workspace::open(&root)
        .map_err(|error| crate::storage_failed(error.to_string()))?;
    // The artifact store is not written by an explanation; the registry needs
    // one because its executors produce evidence when they actually run.
    let artifacts = std::sync::Arc::new(
        arsy_kernel::artifact::FileArtifactStore::open(
            root.join(arsy_code::workspace::ARTIFACTS),
            0,
        )
        .map_err(|error| crate::storage_failed(error.to_string()))?,
    );
    // An explanation, not a turn: nothing here shares a session or task, so a
    // fresh scope is the correct isolation for whatever plan/validation state
    // this registry's contracts describe.
    let registry = arsy_code::operations::registry(
        &workspace,
        artifacts,
        0,
        arsy_code::operations::Reachable::from_config(&config),
        &arsy_kernel::domain::SessionId::new().to_string(),
        // A dry run explains what would happen; it writes nothing anywhere.
        arsy_code::operations::TurnState::default(),
        &[],
    )
    .map_err(|error| crate::storage_failed(error.to_string()))?;
    let contract = registry.contract(&kind).ok_or_else(|| {
        let known: Vec<String> = registry.kinds().map(ToString::to_string).collect();
        // Naming an operation that does not exist is bad input, not a policy
        // denial: exiting 3 would tell a script that policy refused it.
        Diagnostic::error(
            "ARSY-SCH-1001",
            format!("no operation named `{operation}` can be dispatched by this build"),
            format!("known operations: {}", known.join(", ")),
        )
    })?;

    let principal = match actor {
        None => crate::actor(),
        Some("system") => Principal::System,
        Some(named) => Principal::User(named.strip_prefix("user:").unwrap_or(named).to_owned()),
    };
    let context = RiskContext {
        reversible: contract.idempotency == Idempotency::Idempotent,
        workspace: arsy_code::git::cleanliness(&root).unwrap_or(WorkspaceCleanliness::Unknown),
        sandbox: crate::installed_sandbox_assurance(),
    };

    let mut queries = Vec::new();
    for action in &contract.actions {
        let reference = requested_resource(resource, *action, &root)?;
        let requirement = CapabilityRequirement {
            action: *action,
            resource: reference.clone(),
        };
        let request = OperationRequest {
            id: OperationId::new(),
            kind: kind.clone(),
            actor: principal.clone(),
            requirements: vec![requirement.clone()],
            input: Value::Null,
        };
        queries.push((
            *action,
            PolicyQuery {
                actor: principal.clone(),
                operation: kind.clone(),
                requirement,
                operation_digest: request.digest(),
                resource_version: None,
                context,
            },
        ));
    }

    // The same compiled rule set an execution decides on, defaults included:
    // a dry run that built its own would be answering a different question.
    let rules = config.policy_rule_set();
    let mut evaluations = Vec::with_capacity(queries.len());
    let mut worst = RuleEffect::Allow;
    for (action, query) in &queries {
        let outcome = rules.evaluate(query);
        worst = worst.min(effect_of(&outcome.decision));
        evaluations.push(json!({
            "action": action.as_str(),
            "resource": format!("{}:{}", query.requirement.resource.scheme(), query.requirement.resource.value()),
            "decision": describe(&outcome.decision),
            "compile_diagnostics": rules
                .diagnostics()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "trace": outcome
                .trace
                .iter()
                .map(|entry| json!({
                    "source": entry.source.to_string(),
                    "effect": effect_name(entry.effect),
                    "pattern": entry.pattern,
                    "matched": entry.matched,
                    "note": entry.note,
                }))
                .collect::<Vec<_>>(),
        }));
    }

    let (default_effect, default_source) = config.policy_default();
    let report = json!({
        "operation": operation,
        "actor": principal,
        "executed": false,
        "decision": effect_name(worst),
        "context": {
            "reversible": context.reversible,
            "workspace": context.workspace,
            "sandbox_assurance": context.sandbox.as_str(),
        },
        "default_effect": {
            "effect": effect_name(default_effect),
            "source": default_source.to_string(),
        },
        "configured_rules": config.policy_rules().len(),
        "evaluations": evaluations,
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"policy": human(&report)})
    });
    // A dry run reports; it does not fail because the answer was "deny".
    Ok(0)
}

fn effect_of(decision: &PolicyDecision) -> RuleEffect {
    match decision {
        PolicyDecision::Allow(_) => RuleEffect::Allow,
        PolicyDecision::RequireApproval(_) => RuleEffect::RequireApproval,
        PolicyDecision::Deny(_) => RuleEffect::Deny,
    }
}

fn describe(decision: &PolicyDecision) -> Value {
    match decision {
        PolicyDecision::Allow(grant) => json!({
            "effect": "allow",
            "granted_by": grant.source.to_string(),
            "scope": grant
                .scope
                .patterns()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "expires_at_ms": grant.expires_at_ms,
        }),
        PolicyDecision::RequireApproval(request) => json!({
            "effect": "ask",
            "reason": request.reason,
            "intended_effect": request.intended_effect(),
            "scope": request.scope(),
            "operation_digest": request.operation_digest.to_string(),
        }),
        PolicyDecision::Deny(reason) => json!({
            "effect": "deny",
            "reason": reason.message,
            "denied_by": reason.source.map(|source| source.to_string()),
        }),
    }
}

/// `--resource` as written, or the resource this action is about by default.
///
/// The report always names what was evaluated, so an assumed resource is
/// visible rather than silently deciding the answer.
fn requested_resource(
    resource: Option<&str>,
    action: CapabilityAction,
    root: &Path,
) -> Result<ResourceRef, Diagnostic> {
    match resource {
        Some(text) => {
            let (scheme, value) = text
                .split_once(':')
                .ok_or_else(|| usage(format!("`{text}` must be written `<scheme>:<value>`")))?;
            ResourceRef::new(scheme, value)
                .map_err(|error| usage(format!("`{text}` is not a resource reference: {error}")))
        }
        None => {
            let scheme = action.default_scheme();
            let value = if scheme == "file" {
                root.display().to_string()
            } else {
                "*".to_owned()
            };
            ResourceRef::new(scheme, value)
                .map_err(|error| crate::storage_failed(error.to_string()))
        }
    }
}

fn human(report: &Value) -> String {
    let mut text = format!(
        "{} → {}  (nothing was executed)\n  actor: {} · reversible: {} · workspace: {} · sandbox: {}\n  {} configured rule(s); default effect {} from {}\n",
        report["operation"].as_str().unwrap_or("?"),
        report["decision"].as_str().unwrap_or("?"),
        report["actor"],
        report["context"]["reversible"],
        report["context"]["workspace"],
        report["context"]["sandbox_assurance"].as_str().unwrap_or("?"),
        report["configured_rules"],
        report["default_effect"]["effect"].as_str().unwrap_or("?"),
        report["default_effect"]["source"].as_str().unwrap_or("?"),
    );
    for evaluation in report["evaluations"].as_array().unwrap_or(&Vec::new()) {
        text.push_str(&format!(
            "\n  {} over {}\n    {} — {}\n",
            evaluation["action"].as_str().unwrap_or("?"),
            evaluation["resource"].as_str().unwrap_or("?"),
            evaluation["decision"]["effect"].as_str().unwrap_or("?"),
            evaluation["decision"]["reason"]
                .as_str()
                .or_else(|| evaluation["decision"]["granted_by"].as_str())
                .unwrap_or("no reason recorded"),
        ));
        for entry in evaluation["trace"].as_array().unwrap_or(&Vec::new()) {
            if entry["matched"] == Value::Bool(true) {
                text.push_str(&format!(
                    "    matched {} {} {}\n",
                    entry["source"].as_str().unwrap_or("?"),
                    entry["effect"].as_str().unwrap_or("?"),
                    entry["pattern"].as_str().unwrap_or("?"),
                ));
            }
        }
        for diagnostic in evaluation["compile_diagnostics"]
            .as_array()
            .unwrap_or(&Vec::new())
        {
            text.push_str(&format!(
                "    note: {}\n",
                diagnostic.as_str().unwrap_or("?")
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{capability::PolicySource, config::Config};

    #[test]
    fn policy_explain_requires_an_operation() {
        assert!(crate::parse(["policy".to_owned(), "explain".to_owned()]).is_err());
        assert!(crate::parse(["policy".to_owned()]).is_err());
        let parsed = crate::parse(
            ["policy", "explain", "git.status", "--resource", "file:src"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::PolicyExplain {
                operation: "git.status".to_owned(),
                resource: Some("file:src".to_owned()),
                actor: None,
            }
        );
    }

    #[test]
    fn an_action_without_a_rule_is_denied_and_a_default_allow_grants_it() {
        let default_deny = Config::load(&[]).unwrap();
        assert_eq!(default_deny.policy_default().0, RuleEffect::RequireApproval);
        assert_eq!(default_deny.policy_default().1, PolicySource::Enterprise);
        let rules = default_deny.policy_rule_set();
        assert!(rules.rules().is_empty(), "no rule was configured");
        assert_eq!(
            rules.fallback().len(),
            CapabilityAction::ALL.len(),
            "one synthesized catch-all per action, and nothing else"
        );
    }

    #[test]
    fn a_resource_reference_must_name_a_scheme() {
        let root = Path::new(".");
        assert!(requested_resource(Some("nope"), CapabilityAction::FsRead, root).is_err());
        let given =
            requested_resource(Some("file:src/main.rs"), CapabilityAction::FsRead, root).unwrap();
        assert_eq!(given.scheme(), "file");
        assert_eq!(given.value(), "src/main.rs");
        assert_eq!(
            requested_resource(None, CapabilityAction::ProcessExec, root)
                .unwrap()
                .scheme(),
            "process"
        );
    }
}
