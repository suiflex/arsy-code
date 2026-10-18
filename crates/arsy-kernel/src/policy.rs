//! The one place authority is decided.
//!
//! Rules compile to a normalized form, evaluation is deterministic and closed
//! by default, and every decision carries the trace that explains it — which is
//! what `arsy policy explain` prints without running anything.

use crate::{
    capability::{
        CapabilityAction, CapabilityGrant, CapabilityRequirement, PolicySource, ResourcePattern,
        ResourceScope,
    },
    domain::{ApprovalId, GrantId, Principal, StateVersion},
    operation::OperationKind,
    protocol::ApprovalResolution,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
};

/// What a rule does when it matches. Ordered by how much it restricts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Deny,
    RequireApproval,
    Allow,
}

/// Which actor a rule speaks about.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorMatch {
    Any,
    Exactly(Principal),
}

impl ActorMatch {
    pub fn matches(&self, actor: &Principal) -> bool {
        match self {
            Self::Any => true,
            Self::Exactly(expected) => expected == actor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyRule {
    pub source: PolicySource,
    pub effect: RuleEffect,
    pub actor: ActorMatch,
    pub action: CapabilityAction,
    pub pattern: ResourcePattern,
    /// Carried into any grant this rule produces.
    pub expires_at_ms: Option<u64>,
    pub delegation_depth: u32,
    #[serde(default)]
    pub minimum_assurance: SandboxAssurance,
}

impl PolicyRule {
    fn matches(&self, query: &PolicyQuery) -> bool {
        self.actor.matches(&query.actor)
            && self.action == query.requirement.action
            && self.pattern.matches(&query.requirement.resource)
            && query.context.sandbox >= self.minimum_assurance
    }
}

/// A rule that was rewritten during compilation, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub source: PolicySource,
    pub pattern: String,
    pub message: &'static str,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.source, self.pattern, self.message
        )
    }
}

/// Rules in normalized form, ready to evaluate.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuleSet {
    rules: Vec<PolicyRule>,
    /// Consulted only for a query no rule in `rules` matches: what
    /// `policy.default_effect` stands for.
    fallback: Vec<PolicyRule>,
    diagnostics: Vec<Diagnostic>,
}

impl RuleSet {
    /// Normalize rules into a form whose meaning does not depend on the order
    /// they arrived in.
    ///
    /// The one rewrite is the structural rule from the policy spec: a
    /// repository or a session may tighten policy or ask for behaviour, but it
    /// cannot grant itself authority. Such an `Allow` becomes a request for
    /// approval and is reported.
    pub fn compile(rules: impl IntoIterator<Item = PolicyRule>) -> Self {
        let mut compiled = Vec::new();
        let mut diagnostics = Vec::new();
        for mut rule in rules {
            if rule.effect == RuleEffect::Allow && !rule.source.may_grant() {
                diagnostics.push(Diagnostic {
                    source: rule.source,
                    pattern: rule.pattern.to_string(),
                    message: "allow downgraded to approval: this source cannot grant authority",
                });
                rule.effect = RuleEffect::RequireApproval;
            }
            compiled.push(rule);
        }
        // Sorted so that evaluation picks the same rule however the sources
        // were concatenated.
        compiled.sort_by(|left, right| {
            left.source
                .cmp(&right.source)
                .then_with(|| left.effect.cmp(&right.effect))
                .then_with(|| left.actor.cmp(&right.actor))
                .then_with(|| left.action.cmp(&right.action))
                .then_with(|| left.pattern.to_string().cmp(&right.pattern.to_string()))
                .then_with(|| left.expires_at_ms.cmp(&right.expires_at_ms))
                .then_with(|| left.delegation_depth.cmp(&right.delegation_depth))
                .then_with(|| left.minimum_assurance.cmp(&right.minimum_assurance))
        });
        Self {
            rules: compiled,
            fallback: Vec::new(),
            diagnostics,
        }
    }

    /// Answer the queries no rule matches with `defaults`, normalized the
    /// same way.
    ///
    /// A default is what happens to a requirement nothing was written about.
    /// Evaluated beside the rules, it overrode them: a default `allow` at
    /// depth 0 shadowed a rule written to delegate, a default `allow` beat a
    /// rule that asks, and a default `deny` beat every rule that allows. Held
    /// back until no rule matches, it decides only what it was meant to.
    #[must_use]
    pub fn with_fallback(mut self, defaults: impl IntoIterator<Item = PolicyRule>) -> Self {
        let compiled = Self::compile(defaults);
        self.fallback = compiled.rules;
        self.diagnostics.extend(compiled.diagnostics);
        self
    }

    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    pub fn fallback(&self) -> &[PolicyRule] {
        &self.fallback
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Stable digest of the normalized rules used for an authorization.
    pub fn revision(&self) -> StateVersion {
        let bytes =
            serde_json::to_vec(&(&self.rules, &self.fallback)).expect("policy rules serialize");
        StateVersion::from_digest(Sha256::digest(bytes).into())
    }

    /// Decide one query, and say why.
    ///
    /// Deny is looked for first, so adding a deny rule can only ever remove
    /// authority. Silence is a denial: a query neither a rule nor the
    /// fallback speaks to is refused.
    pub fn evaluate(&self, query: &PolicyQuery) -> PolicyOutcome {
        let mut trace = Vec::new();
        let mut first: [Option<&PolicyRule>; 3] = [None, None, None];

        let matched = Self::consider(
            &self.rules,
            query,
            &mut first,
            &mut trace,
            "actor, action, and resource all match",
        );
        if !matched {
            Self::consider(
                &self.fallback,
                query,
                &mut first,
                &mut trace,
                "the default: no rule covers this actor, action, and resource",
            );
        }

        let decision = self.decide(query, first, &mut trace);
        PolicyOutcome { decision, trace }
    }

    /// Record each of `rules` against `query`, keeping the first match of
    /// each effect, and say whether any matched.
    fn consider<'a>(
        rules: &'a [PolicyRule],
        query: &PolicyQuery,
        first: &mut [Option<&'a PolicyRule>; 3],
        trace: &mut Vec<TraceEntry>,
        why: &'static str,
    ) -> bool {
        let mut any = false;
        for rule in rules {
            let matched = rule.matches(query);
            trace.push(TraceEntry {
                source: rule.source,
                effect: rule.effect,
                pattern: rule.pattern.to_string(),
                matched,
                note: if matched {
                    why
                } else {
                    "does not cover this actor, action, or resource"
                },
            });
            if matched {
                any = true;
                let slot = &mut first[effect_slot(rule.effect)];
                if slot.is_none() {
                    *slot = Some(rule);
                }
            }
        }
        any
    }

    /// Evaluate through a caller-owned bounded cache. The key includes the
    /// normalized rules and the complete query, including resource state.
    pub fn evaluate_cached(&self, query: &PolicyQuery, cache: &mut DecisionCache) -> PolicyOutcome {
        if let Some(entry) = cache.entries.iter().find(|entry| {
            entry.rules == self.rules && entry.fallback == self.fallback && entry.query == *query
        }) {
            return entry.outcome.clone();
        }

        let outcome = self.evaluate(query);
        if cache.capacity > 0 {
            if cache.entries.len() == cache.capacity {
                cache.entries.pop_front();
            }
            cache.entries.push_back(CacheEntry {
                rules: self.rules.clone(),
                fallback: self.fallback.clone(),
                query: query.clone(),
                outcome: outcome.clone(),
            });
        }
        outcome
    }

    fn decide(
        &self,
        query: &PolicyQuery,
        first: [Option<&PolicyRule>; 3],
        trace: &mut Vec<TraceEntry>,
    ) -> PolicyDecision {
        if let Some(rule) = first[effect_slot(RuleEffect::Deny)] {
            return PolicyDecision::Deny(DenialReason {
                message: format!("denied by {} rule {}", rule.source, rule.pattern),
                source: Some(rule.source),
            });
        }

        if let Some(rule) = first[effect_slot(RuleEffect::Allow)] {
            if query.requirement.action == CapabilityAction::GitWrite
                && query.context.workspace == WorkspaceCleanliness::Dirty
            {
                trace.push(TraceEntry {
                    source: rule.source,
                    effect: RuleEffect::Allow,
                    pattern: rule.pattern.to_string(),
                    matched: true,
                    note: "allow raised to approval: the workspace has uncommitted changes",
                });
                return PolicyDecision::RequireApproval(approval_from(
                    query,
                    rule,
                    "the workspace has uncommitted changes",
                ));
            }
            // Irreversible work is never waved through on a rule alone.
            if query.context.reversible {
                return PolicyDecision::Allow(grant_from(query, rule));
            }
            trace.push(TraceEntry {
                source: rule.source,
                effect: RuleEffect::Allow,
                pattern: rule.pattern.to_string(),
                matched: true,
                note: "allow raised to approval: the effect is irreversible",
            });
            return PolicyDecision::RequireApproval(approval_from(
                query,
                rule,
                "the effect is irreversible",
            ));
        }

        if let Some(rule) = first[effect_slot(RuleEffect::RequireApproval)] {
            return PolicyDecision::RequireApproval(approval_from(
                query,
                rule,
                "a rule requires review",
            ));
        }

        PolicyDecision::Deny(DenialReason {
            message: "no rule permits this requirement".to_owned(),
            source: None,
        })
    }
}

const fn effect_slot(effect: RuleEffect) -> usize {
    match effect {
        RuleEffect::Deny => 0,
        RuleEffect::Allow => 1,
        RuleEffect::RequireApproval => 2,
    }
}

fn grant_from(query: &PolicyQuery, rule: &PolicyRule) -> CapabilityGrant {
    CapabilityGrant {
        id: GrantId::new(),
        actor: query.actor.clone(),
        action: query.requirement.action,
        scope: ResourceScope::single(rule.pattern.clone()),
        expires_at_ms: rule.expires_at_ms,
        delegation_depth: rule.delegation_depth,
        source: rule.source,
    }
}

fn approval_from(query: &PolicyQuery, rule: &PolicyRule, reason: &str) -> ApprovalRequest {
    ApprovalRequest {
        id: ApprovalId::new(),
        actor: query.actor.clone(),
        operation: query.operation.clone(),
        requirement: query.requirement.clone(),
        operation_digest: query.operation_digest,
        reversible: query.context.reversible,
        expires_at_ms: rule.expires_at_ms,
        delegation_depth: rule.delegation_depth,
        reason: reason.to_owned(),
    }
}

/// What is being asked, by whom, over what.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyQuery {
    pub actor: Principal,
    pub operation: OperationKind,
    pub requirement: CapabilityRequirement,
    /// Binds any approval to this exact operation, so a mutated request cannot
    /// reuse the answer.
    pub operation_digest: StateVersion,
    /// Version of the canonical resource state inspected by policy. A changed
    /// version cannot reuse an earlier cached decision.
    pub resource_version: Option<StateVersion>,
    pub context: RiskContext,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxAssurance {
    #[default]
    None,
    Process,
    Filesystem,
    Full,
}

impl SandboxAssurance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Process => "process",
            Self::Filesystem => "filesystem",
            Self::Full => "full",
        }
    }
}

impl fmt::Display for SandboxAssurance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskContext {
    /// Whether the effect can be undone. Irreversible work never auto-runs.
    pub reversible: bool,
    /// A dirty workspace raises Git mutations to approval so uncommitted work
    /// cannot be overwritten under a broad allow rule.
    pub workspace: WorkspaceCleanliness,
    pub sandbox: SandboxAssurance,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCleanliness {
    Clean,
    Dirty,
    #[default]
    Unknown,
}

impl Default for RiskContext {
    fn default() -> Self {
        Self {
            reversible: true,
            workspace: WorkspaceCleanliness::Unknown,
            sandbox: SandboxAssurance::None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    Allow(CapabilityGrant),
    RequireApproval(ApprovalRequest),
    Deny(DenialReason),
}

impl PolicyDecision {
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub actor: Principal,
    pub operation: OperationKind,
    pub requirement: CapabilityRequirement,
    pub operation_digest: StateVersion,
    pub reversible: bool,
    pub expires_at_ms: Option<u64>,
    pub delegation_depth: u32,
    pub reason: String,
}

impl ApprovalRequest {
    pub fn intended_effect(&self) -> String {
        format!(
            "Run {} with permission to {} {}:{}",
            self.operation,
            self.requirement.action,
            self.requirement.resource.scheme(),
            self.requirement.resource.value()
        )
    }

    pub fn scope(&self) -> String {
        format!(
            "Only {}:{}",
            self.requirement.resource.scheme(),
            self.requirement.resource.value()
        )
    }

    /// The grant an operator's "yes" produces.
    ///
    /// Exactly the requirement that was shown and nothing beside it: the scope
    /// is the literal resource, the depth is zero so the answer cannot be
    /// delegated onward, and the expiry is the approval's own.
    ///
    /// [`ApprovalFlow`] is the durable path — it records who approved what —
    /// and is what a session with an approval ledger should use. This is the
    /// same grant for a caller that has already shown the request to the
    /// operator and holds the answer in hand, so an interactive turn does not
    /// have to keep a ledger in order to act on a "yes".
    pub fn grant(&self) -> Result<CapabilityGrant, ApprovalError> {
        exact_grant(self)
    }

    pub const fn reversibility(&self) -> &'static str {
        if self.reversible {
            "The intended effect is reversible"
        } else {
            "The intended effect is irreversible"
        }
    }
}

/// One-shot resolver for approval requests. The trusted transport supplies the
/// approver identity; untrusted request payloads never get to name it.
pub struct ApprovalFlow {
    capacity: usize,
    pending: BTreeMap<ApprovalId, ApprovalRequest>,
    records: Vec<ApprovalRecord>,
}

impl ApprovalFlow {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            pending: BTreeMap::new(),
            records: Vec::new(),
        }
    }

    pub fn request(&mut self, request: ApprovalRequest) -> Result<(), ApprovalError> {
        if self.pending.contains_key(&request.id) {
            return Err(ApprovalError::Duplicate);
        }
        if self.pending.len() >= self.capacity {
            return Err(ApprovalError::Capacity);
        }
        self.pending.insert(request.id, request);
        Ok(())
    }

    pub fn resolve(
        &mut self,
        resolution: &ApprovalResolution,
        approver: Principal,
    ) -> Result<&ApprovalRecord, ApprovalError> {
        let request = self
            .pending
            .get(&resolution.approval)
            .ok_or(ApprovalError::Unknown)?;
        if request.operation_digest != resolution.operation_digest {
            return Err(ApprovalError::OperationChanged);
        }

        let grant = resolution
            .approved
            .then(|| exact_grant(request))
            .transpose()?;
        let request = self
            .pending
            .remove(&resolution.approval)
            .ok_or(ApprovalError::Unknown)?;
        self.records.push(ApprovalRecord {
            request,
            approver,
            approved: resolution.approved,
            grant,
        });
        Ok(self.records.last().expect("the record was just pushed"))
    }

    pub fn records(&self) -> &[ApprovalRecord] {
        &self.records
    }
}

fn exact_grant(request: &ApprovalRequest) -> Result<CapabilityGrant, ApprovalError> {
    let resource = &request.requirement.resource;
    let pattern = ResourcePattern::new(resource.scheme(), globset::escape(resource.value()))
        .map_err(|_| ApprovalError::InvalidScope)?;
    Ok(CapabilityGrant {
        id: GrantId::new(),
        actor: request.actor.clone(),
        action: request.requirement.action,
        scope: ResourceScope::single(pattern),
        expires_at_ms: request.expires_at_ms,
        delegation_depth: 0,
        source: PolicySource::User,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRecord {
    pub request: ApprovalRequest,
    pub approver: Principal,
    pub approved: bool,
    pub grant: Option<CapabilityGrant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalError {
    Capacity,
    Duplicate,
    Unknown,
    OperationChanged,
    InvalidScope,
}

impl fmt::Display for ApprovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Capacity => "approval queue is full",
            Self::Duplicate => "approval request already exists",
            Self::Unknown => "approval request is unknown or already resolved",
            Self::OperationChanged => "operation changed after approval was requested",
            Self::InvalidScope => "approval scope cannot be represented safely",
        })
    }
}

impl std::error::Error for ApprovalError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenialReason {
    pub message: String,
    pub source: Option<PolicySource>,
}

/// One rule as it was considered. The sequence of these is the explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceEntry {
    pub source: PolicySource,
    pub effect: RuleEffect,
    pub pattern: String,
    pub matched: bool,
    pub note: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyOutcome {
    pub decision: PolicyDecision,
    pub trace: Vec<TraceEntry>,
}

/// A bounded decision cache. Callers choose the bound and lifetime explicitly.
#[derive(Clone, Debug)]
pub struct DecisionCache {
    capacity: usize,
    entries: VecDeque<CacheEntry>,
}

impl DecisionCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
struct CacheEntry {
    rules: Vec<PolicyRule>,
    fallback: Vec<PolicyRule>,
    query: PolicyQuery,
    outcome: PolicyOutcome,
}

impl PolicyOutcome {
    /// The rules that actually bore on the decision.
    pub fn matched(&self) -> impl Iterator<Item = &TraceEntry> {
        self.trace.iter().filter(|entry| entry.matched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ResourceRef;

    fn rule(source: PolicySource, effect: RuleEffect, glob: &str) -> PolicyRule {
        PolicyRule {
            source,
            effect,
            actor: ActorMatch::Any,
            action: CapabilityAction::FsRead,
            pattern: ResourcePattern::new("file", glob).unwrap(),
            expires_at_ms: None,
            delegation_depth: 1,
            minimum_assurance: SandboxAssurance::None,
        }
    }

    fn query(value: &str) -> PolicyQuery {
        PolicyQuery {
            actor: Principal::System,
            operation: OperationKind::new("fs.read").unwrap(),
            requirement: CapabilityRequirement::new(
                CapabilityAction::FsRead,
                ResourceRef::new("file", value).unwrap(),
            ),
            operation_digest: StateVersion::from_digest([0; 32]),
            resource_version: Some(StateVersion::from_digest([1; 32])),
            context: RiskContext::default(),
        }
    }

    #[test]
    fn silence_is_a_denial() {
        let outcome = RuleSet::default().evaluate(&query("/repo/main.rs"));

        assert!(matches!(outcome.decision, PolicyDecision::Deny(_)));
        assert_eq!(outcome.matched().count(), 0);
    }

    #[test]
    fn a_rule_can_require_achieved_sandbox_assurance() {
        let mut guarded = rule(PolicySource::User, RuleEffect::Allow, "/repo/**");
        guarded.minimum_assurance = SandboxAssurance::Full;
        let rules = RuleSet::compile([guarded]);
        let mut request = query("/repo/main.rs");

        assert!(matches!(
            rules.evaluate(&request).decision,
            PolicyDecision::Deny(_)
        ));
        request.context.sandbox = SandboxAssurance::Full;
        assert!(rules.evaluate(&request).decision.is_allow());
    }

    /// A default as `policy.default_effect` compiles it: one catch-all over
    /// the action's whole scheme, at depth 0.
    fn default(effect: RuleEffect) -> PolicyRule {
        let mut catch_all = rule(PolicySource::User, effect, "**");
        catch_all.delegation_depth = 0;
        catch_all
    }

    /// The default decides only what no rule covers. A scoped rule that
    /// delegates keeps its depth beside a default `allow`, whatever its
    /// pattern sorts as; outside that scope the default still answers.
    #[test]
    fn a_default_allow_is_a_fallback_behind_a_scoped_rule_that_delegates() {
        let mut delegating = rule(PolicySource::User, RuleEffect::Allow, "/repo/**");
        delegating.delegation_depth = 2;
        let rules = RuleSet::compile([delegating]).with_fallback([default(RuleEffect::Allow)]);

        match rules.evaluate(&query("/repo/main.rs")).decision {
            PolicyDecision::Allow(grant) => assert_eq!(grant.delegation_depth, 2),
            other => panic!("expected an allow, got {other:?}"),
        }
        match rules.evaluate(&query("/elsewhere/notes.md")).decision {
            PolicyDecision::Allow(grant) => assert_eq!(grant.delegation_depth, 0),
            other => panic!("expected the default allow, got {other:?}"),
        }
    }

    /// A rule that asks is asked beside a default `allow`, and a rule that
    /// allows applies beside a default `deny`: the default never overrides
    /// what a rule says about the same requirement.
    #[test]
    fn a_default_never_overrides_a_matching_rule() {
        let asks = RuleSet::compile([rule(
            PolicySource::User,
            RuleEffect::RequireApproval,
            "/repo/**",
        )])
        .with_fallback([default(RuleEffect::Allow)]);
        assert!(matches!(
            asks.evaluate(&query("/repo/main.rs")).decision,
            PolicyDecision::RequireApproval(_)
        ));

        let allows = RuleSet::compile([rule(PolicySource::User, RuleEffect::Allow, "/repo/**")])
            .with_fallback([default(RuleEffect::Deny)]);
        assert!(allows.evaluate(&query("/repo/main.rs")).decision.is_allow());
        assert!(matches!(
            allows.evaluate(&query("/elsewhere/notes.md")).decision,
            PolicyDecision::Deny(_)
        ));

        // A rule that denies still beats a rule that allows, fallback or not.
        let denies = RuleSet::compile([
            rule(PolicySource::User, RuleEffect::Allow, "/repo/**"),
            rule(PolicySource::User, RuleEffect::Deny, "/repo/secrets/**"),
        ])
        .with_fallback([default(RuleEffect::Allow)]);
        assert!(matches!(
            denies.evaluate(&query("/repo/secrets/key")).decision,
            PolicyDecision::Deny(_)
        ));
    }

    /// The fallback is part of what a decision was made under: a cache keyed
    /// on the rules alone would answer for a different default.
    #[test]
    fn the_fallback_is_part_of_the_cache_key_and_the_revision() {
        let allow = RuleSet::default().with_fallback([default(RuleEffect::Allow)]);
        let deny = RuleSet::default().with_fallback([default(RuleEffect::Deny)]);
        assert_ne!(allow.revision(), deny.revision());

        let mut cache = DecisionCache::new(4);
        let request = query("/repo/main.rs");
        assert!(allow
            .evaluate_cached(&request, &mut cache)
            .decision
            .is_allow());
        assert!(matches!(
            deny.evaluate_cached(&request, &mut cache).decision,
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn a_deny_beats_an_allow_from_a_higher_source() {
        let rules = RuleSet::compile([
            rule(PolicySource::Enterprise, RuleEffect::Allow, "/repo/**"),
            rule(PolicySource::Session, RuleEffect::Deny, "/repo/secrets/**"),
        ]);

        assert!(rules.evaluate(&query("/repo/main.rs")).decision.is_allow());
        assert!(matches!(
            rules.evaluate(&query("/repo/secrets/key")).decision,
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn a_workspace_cannot_grant_itself_authority() {
        let rules = RuleSet::compile([rule(PolicySource::Workspace, RuleEffect::Allow, "/**")]);

        assert!(matches!(
            rules.evaluate(&query("/repo/main.rs")).decision,
            PolicyDecision::RequireApproval(_)
        ));
        assert_eq!(rules.diagnostics().len(), 1);
    }

    #[test]
    fn irreversible_work_is_raised_to_approval() {
        let rules = RuleSet::compile([rule(PolicySource::User, RuleEffect::Allow, "/repo/**")]);
        let mut asked = query("/repo/main.rs");
        asked.context.reversible = false;

        let outcome = rules.evaluate(&asked);

        assert!(matches!(
            outcome.decision,
            PolicyDecision::RequireApproval(_)
        ));
        assert!(outcome
            .matched()
            .any(|entry| entry.note.contains("irreversible")));
    }

    #[test]
    fn dirty_workspace_raises_git_mutation_to_approval() {
        let mut git_rule = rule(PolicySource::User, RuleEffect::Allow, "*");
        git_rule.action = CapabilityAction::GitWrite;
        git_rule.pattern = ResourcePattern::new("git", "*").unwrap();
        let rules = RuleSet::compile([git_rule]);
        let mut asked = query("unused");
        asked.operation = OperationKind::new("git.commit").unwrap();
        asked.requirement = CapabilityRequirement::new(
            CapabilityAction::GitWrite,
            ResourceRef::new("git", ".").unwrap(),
        );
        asked.context.workspace = WorkspaceCleanliness::Dirty;

        let outcome = rules.evaluate(&asked);

        assert!(matches!(
            outcome.decision,
            PolicyDecision::RequireApproval(_)
        ));
        assert!(outcome
            .matched()
            .any(|entry| entry.note.contains("uncommitted changes")));
    }

    #[test]
    fn the_trace_names_the_rule_that_decided() {
        let rules = RuleSet::compile([
            rule(PolicySource::User, RuleEffect::Allow, "/repo/**"),
            rule(PolicySource::User, RuleEffect::Deny, "/repo/.env"),
            rule(PolicySource::User, RuleEffect::Allow, "/other/**"),
        ]);

        let outcome = rules.evaluate(&query("/repo/.env"));

        let deciding: Vec<_> = outcome
            .matched()
            .filter(|entry| entry.effect == RuleEffect::Deny)
            .collect();
        assert_eq!(deciding.len(), 1);
        assert_eq!(deciding[0].pattern, "file:/repo/.env");
        // Rules that did not bear on it are still reported, as considered.
        assert_eq!(outcome.trace.len(), 3);
    }

    #[test]
    fn an_allow_grants_only_its_own_pattern() {
        let rules = RuleSet::compile([rule(PolicySource::User, RuleEffect::Allow, "/repo/src/**")]);

        let PolicyDecision::Allow(grant) = rules.evaluate(&query("/repo/src/main.rs")).decision
        else {
            panic!("expected an allow");
        };
        assert!(!grant
            .scope
            .admits(&ResourceRef::new("file", "/etc/passwd").unwrap()));
    }

    #[test]
    fn cache_keys_on_complete_inputs_and_resource_version() {
        let rules = RuleSet::compile([rule(PolicySource::User, RuleEffect::Allow, "/repo/**")]);
        let mut cache = DecisionCache::new(2);
        let first = query("/repo/main.rs");
        let mut changed_resource = first.clone();
        changed_resource.resource_version = Some(StateVersion::from_digest([2; 32]));
        let changed_input = query("/repo/other.rs");

        rules.evaluate_cached(&first, &mut cache);
        rules.evaluate_cached(&first, &mut cache);
        assert_eq!(cache.len(), 1);
        rules.evaluate_cached(&changed_resource, &mut cache);
        assert_eq!(cache.len(), 2);
        rules.evaluate_cached(&changed_input, &mut cache);
        assert_eq!(cache.len(), 2, "the cache must remain bounded");
    }
}
