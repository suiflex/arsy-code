//! Structured safety review and responsibility-based agent roles.
//!
//! Policy decides what authority exists. This layer may only narrow that
//! answer before dispatch; it never creates a grant or turns a denial into an
//! allow.

use crate::{
    capability::CapabilityAction,
    domain::{ResourceRef, StateVersion},
    operation::OperationKind,
    orchestration::{Budget, WorkspaceRequirement},
    policy::{SandboxAssurance, WorkspaceCleanliness},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const REVIEW_SCHEMA_VERSION: u32 = 1;
pub const ROLE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Trusted,
    Untrusted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    CapabilityExpansion,
    ScopeEscape,
    ForbiddenNetwork,
    CredentialUse,
    SystemModification,
    Destructive,
    Irreversible,
    DirtyWorkspace,
    UnknownSandbox,
    PolicyUncertainty,
}

/// Redacted, replayable input to an independent safety reviewer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SafetyReviewEnvelope {
    pub schema: u32,
    /// Digest, not prompt text: repository content cannot become reviewer
    /// instructions through this field.
    pub intent_digest: StateVersion,
    pub operation: OperationKind,
    pub operation_digest: StateVersion,
    pub capabilities: Vec<CapabilityAction>,
    pub targets: Vec<ResourceRef>,
    pub reversible: bool,
    pub workspace: WorkspaceCleanliness,
    pub sandbox: SandboxAssurance,
    pub trust: TrustState,
    pub flags: Vec<RiskFlag>,
    pub policy_revision: StateVersion,
    pub reviewer_version: String,
    pub expires_at_ms: u64,
}

impl SafetyReviewEnvelope {
    pub fn cache_key(&self, unattended: bool) -> Result<StateVersion, serde_json::Error> {
        use sha2::{Digest, Sha256};
        let mut input = self.clone();
        // Expiry bounds reuse but does not change the decision. Including the
        // freshly computed timestamp would make identical calls always miss.
        input.expires_at_ms = 0;
        serde_json::to_vec(&(input, unattended))
            .map(|bytes| StateVersion::from_digest(Sha256::digest(bytes).into()))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyDecision {
    Allow,
    RequireApproval,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SafetyReviewResult {
    pub schema: u32,
    pub decision: SafetyDecision,
    pub reasons: Vec<String>,
    pub reviewer: String,
    pub latency_ms: u64,
    pub cost_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SafetyAuditRecord {
    pub envelope: SafetyReviewEnvelope,
    pub result: SafetyReviewResult,
}

/// Exact-input, bounded safety decisions. Expired entries are never reused.
#[derive(Clone, Debug)]
pub struct SafetyReviewCache {
    capacity: usize,
    entries: VecDeque<(StateVersion, u64, SafetyReviewResult)>,
}

impl SafetyReviewCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    pub fn review(
        &mut self,
        envelope: &SafetyReviewEnvelope,
        unattended: bool,
        now_ms: u64,
    ) -> Result<SafetyReviewResult, serde_json::Error> {
        let key = envelope.cache_key(unattended)?;
        self.entries.retain(|(_, expiry, _)| *expiry > now_ms);
        if let Some((_, _, result)) = self.entries.iter().find(|(cached, _, _)| *cached == key) {
            return Ok(result.clone());
        }
        let result = review(envelope, unattended);
        if self.capacity > 0 && envelope.expires_at_ms > now_ms {
            if self.entries.len() == self.capacity {
                self.entries.pop_front();
            }
            self.entries
                .push_back((key, envelope.expires_at_ms, result.clone()));
        }
        Ok(result)
    }
}

/// Deterministic review runs after hard policy and before approval-mode
/// conversion.
///
/// Unattended, anything that would need a person is denied, including an
/// irreversible contract, a dirty workspace, or no sandbox.
///
/// Attended, this is Auto mode's gate, and Auto never stops to ask: a risky
/// action is denied and the model is told why, so it can take another route.
/// Risk here is what the action itself does — delete, touch credentials,
/// modify the system, escape scope. The environment (a dirty workspace, no
/// sandbox, a contract that cannot be undone) describes every call in the
/// session equally, so it is recorded in the flags for the audit but does
/// not decide; otherwise Auto in an ordinary repository would refuse
/// everything.
pub fn review(envelope: &SafetyReviewEnvelope, unattended: bool) -> SafetyReviewResult {
    let deny = envelope.flags.iter().find(|flag| {
        matches!(
            flag,
            RiskFlag::CapabilityExpansion
                | RiskFlag::ScopeEscape
                | RiskFlag::ForbiddenNetwork
                | RiskFlag::PolicyUncertainty
        )
    });
    if let Some(flag) = deny {
        return result(SafetyDecision::Deny, format!("hard risk: {flag:?}"));
    }
    if !unattended {
        return review_attended(envelope);
    }

    let needs_person = !envelope.reversible
        || envelope.workspace != WorkspaceCleanliness::Clean
        || envelope.sandbox == SandboxAssurance::None
        || envelope.trust != TrustState::Trusted
        || envelope.flags.iter().any(|flag| {
            matches!(
                flag,
                RiskFlag::CredentialUse
                    | RiskFlag::SystemModification
                    | RiskFlag::Destructive
                    | RiskFlag::Irreversible
                    | RiskFlag::DirtyWorkspace
                    | RiskFlag::UnknownSandbox
            )
        });
    if needs_person {
        return result(
            SafetyDecision::Deny,
            "risk needs human approval, but this run is unattended",
        );
    }
    result(
        SafetyDecision::Allow,
        "deterministic low-risk review passed",
    )
}

/// The attended half of [`review`], once hard risk has been ruled out.
fn review_attended(envelope: &SafetyReviewEnvelope) -> SafetyReviewResult {
    if envelope.trust != TrustState::Trusted {
        return result(
            SafetyDecision::Deny,
            "blocked: the intent behind this call is not trusted",
        );
    }
    let risky: Vec<&RiskFlag> = envelope
        .flags
        .iter()
        .filter(|flag| {
            matches!(
                flag,
                RiskFlag::CredentialUse | RiskFlag::SystemModification | RiskFlag::Destructive
            )
        })
        .collect();
    if !risky.is_empty() {
        return result(
            SafetyDecision::Deny,
            format!("blocked: risky action ({risky:?})"),
        );
    }
    result(
        SafetyDecision::Allow,
        "deterministic per-action review passed",
    )
}

fn result(decision: SafetyDecision, reason: impl Into<String>) -> SafetyReviewResult {
    SafetyReviewResult {
        schema: REVIEW_SCHEMA_VERSION,
        decision,
        reasons: vec![reason.into()],
        reviewer: "deterministic-v1".into(),
        latency_ms: 0,
        cost_micros: 0,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Explorer,
    Planner,
    Implementer,
    Debugger,
    TestRunner,
    Reviewer,
    Integrator,
    SafetyReviewer,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoleContract {
    pub schema: u32,
    pub role: AgentRole,
    pub capabilities: Vec<CapabilityAction>,
    pub workspace: WorkspaceRequirement,
    pub budget: Budget,
    pub result_schema: String,
    pub delegation_depth: u32,
    pub may_write: bool,
    pub may_execute: bool,
}

impl AgentRole {
    pub fn contract(self) -> RoleContract {
        use CapabilityAction::{
            DebugAttach, DebugLaunch, FsRead, FsWrite, GitRead, GitWrite, ProcessExec,
        };
        let (capabilities, workspace, may_write, may_execute) = match self {
            Self::Explorer | Self::Planner | Self::Reviewer => (
                vec![FsRead, GitRead],
                WorkspaceRequirement::ReadOnlySnapshot,
                false,
                false,
            ),
            Self::Implementer => (
                vec![FsRead, FsWrite, GitRead, ProcessExec],
                WorkspaceRequirement::IsolatedWriter,
                true,
                true,
            ),
            Self::Debugger => (
                vec![FsRead, ProcessExec, DebugLaunch, DebugAttach],
                WorkspaceRequirement::ReadOnlySnapshot,
                false,
                true,
            ),
            Self::TestRunner => (
                vec![FsRead, ProcessExec],
                WorkspaceRequirement::ReadOnlySnapshot,
                false,
                true,
            ),
            Self::Integrator => (
                vec![FsRead, FsWrite, GitRead, GitWrite],
                WorkspaceRequirement::IsolatedWriter,
                true,
                false,
            ),
            Self::SafetyReviewer => (
                Vec::new(),
                WorkspaceRequirement::ReadOnlySnapshot,
                false,
                false,
            ),
        };
        RoleContract {
            schema: ROLE_SCHEMA_VERSION,
            role: self,
            capabilities,
            workspace,
            budget: Budget::default(),
            result_schema: "application/json".into(),
            delegation_depth: 0,
            may_write,
            may_execute,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> SafetyReviewEnvelope {
        SafetyReviewEnvelope {
            schema: REVIEW_SCHEMA_VERSION,
            intent_digest: StateVersion::from_digest([1; 32]),
            operation: OperationKind::new("fs.write").unwrap(),
            operation_digest: StateVersion::from_digest([2; 32]),
            capabilities: vec![CapabilityAction::FsWrite],
            targets: vec![ResourceRef::new("file", "/repo/src/lib.rs").unwrap()],
            reversible: true,
            workspace: WorkspaceCleanliness::Clean,
            sandbox: SandboxAssurance::Filesystem,
            trust: TrustState::Trusted,
            flags: Vec::new(),
            policy_revision: StateVersion::from_digest([3; 32]),
            reviewer_version: "rules-v1".into(),
            expires_at_ms: 10,
        }
    }

    #[test]
    fn safety_review_only_narrows() {
        assert_eq!(review(&envelope(), false).decision, SafetyDecision::Allow);

        for flag in [
            RiskFlag::CapabilityExpansion,
            RiskFlag::ScopeEscape,
            RiskFlag::ForbiddenNetwork,
            RiskFlag::PolicyUncertainty,
        ] {
            let mut risky = envelope();
            risky.flags.push(flag);
            assert_eq!(review(&risky, false).decision, SafetyDecision::Deny);
        }

        let mut credential = envelope();
        credential.flags.push(RiskFlag::CredentialUse);
        assert_eq!(review(&credential, false).decision, SafetyDecision::Deny);
        assert_eq!(review(&credential, true).decision, SafetyDecision::Deny);
    }

    /// Attended review is Auto mode's gate. It never asks: an action is either
    /// allowed or blocked. What blocks it is what the action does, not the
    /// state of the session every action shares.
    #[test]
    fn attended_review_blocks_risky_actions_and_ignores_the_environment() {
        let mut everyday = envelope();
        everyday.reversible = false;
        everyday.workspace = WorkspaceCleanliness::Dirty;
        everyday.sandbox = SandboxAssurance::None;
        everyday.flags = vec![
            RiskFlag::Irreversible,
            RiskFlag::DirtyWorkspace,
            RiskFlag::UnknownSandbox,
        ];
        assert_eq!(review(&everyday, false).decision, SafetyDecision::Allow);
        // Unattended keeps the strict reading of the same envelope.
        assert_eq!(review(&everyday, true).decision, SafetyDecision::Deny);

        for flag in [
            RiskFlag::CredentialUse,
            RiskFlag::SystemModification,
            RiskFlag::Destructive,
            RiskFlag::ScopeEscape,
            RiskFlag::ForbiddenNetwork,
        ] {
            let mut risky = everyday.clone();
            risky.flags.push(flag);
            let decided = review(&risky, false);
            assert_eq!(decided.decision, SafetyDecision::Deny, "{flag:?}");
            assert!(!decided.reasons.is_empty());
        }

        let mut untrusted = envelope();
        untrusted.trust = TrustState::Untrusted;
        assert_eq!(review(&untrusted, false).decision, SafetyDecision::Deny);

        // No envelope makes an attended review ask.
        for workspace in [WorkspaceCleanliness::Clean, WorkspaceCleanliness::Dirty] {
            let mut any = envelope();
            any.workspace = workspace;
            assert_ne!(
                review(&any, false).decision,
                SafetyDecision::RequireApproval
            );
        }
    }

    #[test]
    fn safety_reviewer_has_no_execution_authority() {
        let contract = AgentRole::SafetyReviewer.contract();
        assert!(contract.capabilities.is_empty());
        assert!(!contract.may_write);
        assert!(!contract.may_execute);
    }

    #[test]
    fn cache_is_exact_and_expires() {
        let mut cache = SafetyReviewCache::new(2);
        let first = envelope();
        assert_eq!(
            cache.review(&first, false, 1).unwrap().decision,
            SafetyDecision::Allow
        );
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.review(&first, false, 2).unwrap().decision,
            SafetyDecision::Allow
        );
        assert_eq!(cache.entries.len(), 1, "an exact hit is reused");

        let mut renewed = first.clone();
        renewed.expires_at_ms += 1;
        cache.review(&renewed, false, 2).unwrap();
        assert_eq!(cache.entries.len(), 1, "expiry is eviction metadata");

        let mut changed = first.clone();
        changed.policy_revision = StateVersion::from_digest([9; 32]);
        cache.review(&changed, false, 2).unwrap();
        assert_eq!(cache.entries.len(), 2, "policy changes invalidate the key");

        cache.review(&first, false, first.expires_at_ms).unwrap();
        assert!(cache.entries.is_empty(), "expired decisions are removed");
    }
}
