//! Choosing a model, under policy.
//!
//! See `docs/09-model-provider-layer.md`. Routing decides *between* models that
//! policy already allows; it never widens that set. Three rules follow from
//! that, and they are why this is a module rather than a sort call:
//!
//! * **A pin is a preference, not an exemption.** Pinning a model that policy
//!   does not allow is refused, with the ceiling that refused it named.
//! * **Disabling routing does not disable policy.** It falls back to the
//!   configured default, which is filtered exactly as a routed choice is.
//! * **The decision says why.** Selecting a model without saying which
//!   criterion decided it is unreviewable, so every decision carries its
//!   reasons and the candidates it rejected.

use crate::{
    model_profile::{CapabilityState, ModelCapability},
    provider::ModelKey,
    safety::AgentRole,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What the turn is for. The harness distinguishes exactly these two today —
/// a person waiting at a terminal, and a pipeline that is not — so those are
/// the classes, rather than a taxonomy nothing produces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    /// Someone is watching the stream: latency decides.
    Interactive,
    /// Nobody is waiting: cost decides.
    Batch,
}

/// One model routing may choose, with whatever is known about it.
///
/// Every measured field is optional because it is measured: a model nothing
/// has been sent to has no latency, and inventing one would make the ranking a
/// fiction. A criterion with no data does not discriminate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub key: ModelKey,
    pub capabilities: BTreeMap<String, ModelCapability>,
    /// Region the endpoint serves from, when configuration states one.
    pub residency: Option<String>,
    /// Micro-units per thousand output tokens, when configuration states it.
    pub cost_micros_per_1k: Option<u64>,
    pub context_window: Option<u64>,
    pub modalities: BTreeSet<String>,
    pub provider_features: BTreeSet<String>,
}

impl Candidate {
    fn supports(&self, capability: &str) -> bool {
        self.capabilities
            .get(capability)
            .is_some_and(|value| value.state == CapabilityState::Supported)
    }
}

/// What has actually been observed, per model.
///
/// Rolling means: a model that was slow once and fast fifty times ranks on the
/// fifty, and a model with one sample is not treated as if it had many.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Observations {
    samples: BTreeMap<ModelKey, Sample>,
    records: Vec<Observation>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Sample {
    turns: u64,
    failures: u64,
    latency_total_ms: u64,
    cost_total_micros: u64,
}

/// One dated, attributable measurement. Quality is a verified outcome from an
/// eval or check, never the model's own completion claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub key: ModelKey,
    pub observed_at_ms: u64,
    pub endpoint_version: String,
    pub sample_size: u64,
    pub eval_suite: String,
    pub role: Option<AgentRole>,
    pub task_class: TaskClass,
    pub latency_ms: u64,
    pub cost_micros: u64,
    pub verified_success: bool,
}

impl Observations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one finished turn.
    pub fn record(&mut self, key: &ModelKey, latency_ms: u64, cost_micros: u64, succeeded: bool) {
        self.record_observation(Observation {
            key: key.clone(),
            observed_at_ms: 0,
            endpoint_version: String::new(),
            sample_size: 1,
            eval_suite: "runtime".into(),
            role: None,
            task_class: TaskClass::Interactive,
            latency_ms,
            cost_micros,
            verified_success: succeeded,
        });
    }

    pub fn record_observation(&mut self, observation: Observation) {
        let key = observation.key.clone();
        let sample_size = observation.sample_size.max(1);
        let sample = self.samples.entry(key.clone()).or_default();
        sample.turns = sample.turns.saturating_add(sample_size);
        if !observation.verified_success {
            sample.failures = sample.failures.saturating_add(sample_size);
        }
        sample.latency_total_ms = sample
            .latency_total_ms
            .saturating_add(observation.latency_ms.saturating_mul(sample_size));
        sample.cost_total_micros = sample
            .cost_total_micros
            .saturating_add(observation.cost_micros.saturating_mul(sample_size));
        self.records.push(observation);
    }

    pub fn records(&self) -> &[Observation] {
        &self.records
    }

    fn scoped(&self, key: &ModelKey, role: Option<AgentRole>, task: TaskClass) -> Option<Sample> {
        let mut sample = Sample::default();
        for observation in self.records.iter().filter(|observation| {
            observation.key == *key && observation.role == role && observation.task_class == task
        }) {
            let count = observation.sample_size.max(1);
            sample.turns = sample.turns.saturating_add(count);
            if !observation.verified_success {
                sample.failures = sample.failures.saturating_add(count);
            }
            sample.latency_total_ms = sample
                .latency_total_ms
                .saturating_add(observation.latency_ms.saturating_mul(count));
            sample.cost_total_micros = sample
                .cost_total_micros
                .saturating_add(observation.cost_micros.saturating_mul(count));
        }
        (sample.turns > 0).then_some(sample)
    }

    pub fn turns(&self, key: &ModelKey) -> u64 {
        self.samples.get(key).map_or(0, |sample| sample.turns)
    }

    /// Mean latency, or `None` when nothing has been measured.
    pub fn mean_latency_ms(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.latency_total_ms / sample.turns)
    }

    pub fn mean_cost_micros(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.cost_total_micros / sample.turns)
    }

    /// Failures per thousand turns. Higher is worse; `None` without samples.
    pub fn failure_rate(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.failures.saturating_mul(1_000) / sample.turns)
    }
}

/// The policy the choice happens inside.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Constraints {
    /// Providers policy allows. `None` is no ceiling.
    ///
    /// An `Option`, not an empty set, because the two are opposite answers: a
    /// layer that wrote `allowed = []`, or two layers whose allowlists
    /// intersect to nothing, permit no provider at all — and reading that as
    /// "unconstrained" routes to an endpoint the configuration forbids.
    pub allowed_providers: Option<BTreeSet<String>>,
    /// Models policy allows. `None` is no ceiling; see `allowed_providers`.
    pub allowed_models: Option<BTreeSet<String>>,
    /// Regions policy allows. `None` is no ceiling.
    pub residency: Option<BTreeSet<String>>,
    /// Capabilities the turn cannot proceed without.
    pub required_capabilities: BTreeSet<String>,
    /// Cap on the mean cost of a turn.
    pub max_cost_micros: Option<u64>,
    pub min_context_tokens: Option<u64>,
    pub required_modalities: BTreeSet<String>,
    pub required_provider_features: BTreeSet<String>,
}

impl Constraints {
    /// Why this candidate is not eligible, or `None` when it is.
    fn rejects(&self, candidate: &Candidate) -> Option<String> {
        if self
            .allowed_providers
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&candidate.key.provider))
        {
            return Some("provider.allowed does not include it".to_owned());
        }
        if self
            .allowed_models
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&candidate.key.model))
        {
            return Some("model.allowed does not include it".to_owned());
        }
        if let Some(residency) = &self.residency {
            match &candidate.residency {
                // An unstated region cannot be shown to satisfy a residency
                // ceiling, so it does not.
                None => return Some("its residency is unstated".to_owned()),
                Some(region) if !residency.contains(region) => {
                    return Some(format!("its region `{region}` is not allowed"))
                }
                Some(_) => {}
            }
        }
        for capability in &self.required_capabilities {
            if !candidate.supports(capability) {
                return Some(format!("it does not support `{capability}`"));
            }
        }
        if self
            .min_context_tokens
            .is_some_and(|minimum| candidate.context_window.is_none_or(|size| size < minimum))
        {
            return Some("its context window is missing or too small".to_owned());
        }
        if let Some(modality) = self
            .required_modalities
            .iter()
            .find(|modality| !candidate.modalities.contains(*modality))
        {
            return Some(format!("it does not support `{modality}` input"));
        }
        if let Some(feature) = self
            .required_provider_features
            .iter()
            .find(|feature| !candidate.provider_features.contains(*feature))
        {
            return Some(format!("its provider does not support `{feature}`"));
        }
        if let (Some(cap), Some(cost)) = (self.max_cost_micros, candidate.cost_micros_per_1k) {
            if cost > cap {
                return Some(format!("its cost {cost} exceeds the {cap} cap"));
            }
        }
        None
    }
}

/// A candidate that did not survive the filter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Excluded {
    pub key: ModelKey,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Decision {
    Routed {
        key: ModelKey,
        /// In the order they were applied.
        reasons: Vec<String>,
        excluded: Vec<Excluded>,
    },
    /// Routing was off, or a pin was honoured. Still policy-filtered.
    Chosen {
        key: ModelKey,
        why: &'static str,
        excluded: Vec<Excluded>,
    },
    Refused {
        reason: String,
        excluded: Vec<Excluded>,
    },
}

impl Decision {
    pub fn key(&self) -> Option<&ModelKey> {
        match self {
            Self::Routed { key, .. } | Self::Chosen { key, .. } => Some(key),
            Self::Refused { .. } => None,
        }
    }

    pub fn excluded(&self) -> &[Excluded] {
        match self {
            Self::Routed { excluded, .. }
            | Self::Chosen { excluded, .. }
            | Self::Refused { excluded, .. } => excluded,
        }
    }
}

/// What the operator asked for, beside what policy allows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preference {
    /// A model the operator pinned. Filtered like any other.
    pub pinned: Option<ModelKey>,
    /// The configured default, used when routing is off.
    pub default: Option<ModelKey>,
    /// `false` disables ranking, not policy.
    pub route: bool,
    pub task: Option<TaskClass>,
    pub role: Option<AgentRole>,
}

/// Decide which model a turn uses.
///
/// The filter runs first and identically for every path, so no preference can
/// reach a model policy excluded. Ranking is deterministic: equal candidates
/// are broken by provider then model, never by iteration order.
///
/// `health` is the model-health port. Its probe state is a *tie-break*
/// penalty only: an unreachable model loses a tie to a reachable one, but a
/// measured turn record always outranks it. Pass an empty
/// [`crate::pulse::ProbeObservations`] when no health channel is available.
pub fn decide(
    candidates: &[Candidate],
    constraints: &Constraints,
    observations: &Observations,
    health: &dyn crate::pulse::HealthProbe,
    preference: &Preference,
) -> Decision {
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for candidate in candidates {
        match constraints.rejects(candidate) {
            Some(reason) => excluded.push(Excluded {
                key: candidate.key.clone(),
                reason,
            }),
            None => eligible.push(candidate),
        }
    }

    if let Some(pinned) = &preference.pinned {
        return match eligible.iter().find(|candidate| candidate.key == *pinned) {
            Some(candidate) => Decision::Chosen {
                key: candidate.key.clone(),
                why: "pinned by the operator",
                excluded,
            },
            None => {
                let reason = excluded
                    .iter()
                    .find(|entry| entry.key == *pinned)
                    .map_or_else(
                        || format!("`{}/{}` is not configured", pinned.provider, pinned.model),
                        |entry| {
                            format!(
                                "`{}/{}` is pinned but {}",
                                pinned.provider, pinned.model, entry.reason
                            )
                        },
                    );
                Decision::Refused { reason, excluded }
            }
        };
    }

    if !preference.route {
        let Some(default) = &preference.default else {
            return Decision::Refused {
                reason: "routing is disabled and no default model is configured".to_owned(),
                excluded,
            };
        };
        return match eligible.iter().find(|candidate| candidate.key == *default) {
            Some(candidate) => Decision::Chosen {
                key: candidate.key.clone(),
                why: "the configured default, with routing disabled",
                excluded,
            },
            None => Decision::Refused {
                reason: format!(
                    "routing is disabled and the default `{}/{}` is not allowed",
                    default.provider, default.model
                ),
                excluded,
            },
        };
    }

    if eligible.is_empty() {
        return Decision::Refused {
            reason: "no configured model satisfies the resolved policy".to_owned(),
            excluded,
        };
    }

    let task = preference.task.unwrap_or(TaskClass::Interactive);
    let mut ranked: Vec<&Candidate> = eligible;
    ranked.sort_by(|left, right| {
        rank(left, observations, health, preference.role, task)
            .cmp(&rank(right, observations, health, preference.role, task))
            .then(left.key.provider.cmp(&right.key.provider))
            .then(left.key.model.cmp(&right.key.model))
    });
    let winner = ranked[0];
    Decision::Routed {
        key: winner.key.clone(),
        reasons: explain(winner, observations, health, preference.role, task),
        excluded,
    }
}

/// The sort key. Lower is better, and every component is `Option`-shaped so a
/// criterion with no measurement neither helps nor hurts.
///
/// Reliability leads: a cheap model that fails is not cheap. Then the class's
/// own priority, then the other one. The health-probe penalty is a
/// tie-break *after* the measured criteria — an unreachable model loses a tie
/// to a reachable one but never overrides a measured turn record. Then a
/// stable fallback.
fn rank(
    candidate: &Candidate,
    observations: &Observations,
    health: &dyn crate::pulse::HealthProbe,
    role: Option<AgentRole>,
    task: TaskClass,
) -> (u64, u64, u64, u8, usize) {
    let sample = observations.scoped(&candidate.key, role, task);
    let failures = sample
        .filter(|sample| sample.turns > 0)
        .map(|sample| sample.failures.saturating_mul(1_000) / sample.turns)
        .unwrap_or(0);
    let latency = sample
        .filter(|sample| sample.turns > 0)
        .map(|sample| sample.latency_total_ms / sample.turns)
        .unwrap_or(u64::MAX);
    let cost = sample
        .filter(|sample| sample.turns > 0)
        .map(|sample| sample.cost_total_micros / sample.turns)
        .or(candidate.cost_micros_per_1k)
        .unwrap_or(u64::MAX);
    let (first, second) = match task {
        TaskClass::Interactive => (latency, cost),
        TaskClass::Batch => (cost, latency),
    };
    // Probe state breaks ties only; it sorts after failures, latency and
    // cost so a measured record always outranks it.
    let penalty = health.probe_state(&candidate.key).penalty();
    // A model with no measurements at all sorts behind one with any, rather
    // than winning on a `u64::MAX` that happens to tie.
    let unmeasured = usize::from(sample.is_none());
    (failures, first, second, penalty, unmeasured)
}

fn explain(
    candidate: &Candidate,
    observations: &Observations,
    health: &dyn crate::pulse::HealthProbe,
    role: Option<AgentRole>,
    task: TaskClass,
) -> Vec<String> {
    let mut reasons = vec![format!(
        "ranked for a {} turn{}",
        match task {
            TaskClass::Interactive => "latency-sensitive",
            TaskClass::Batch => "cost-sensitive",
        },
        role.map_or_else(String::new, |role| format!(" as {role:?}"))
    )];
    match observations.scoped(&candidate.key, role, task) {
        None => {
            reasons.push("nothing has been measured for this role and task class yet".to_owned())
        }
        Some(sample) => {
            let turns = sample.turns;
            reasons.push(format!("measured over {turns} turn(s)"));
            reasons.push(format!(
                "mean latency {} ms",
                sample.latency_total_ms / turns
            ));
            reasons.push(format!(
                "mean cost {} micro-units",
                sample.cost_total_micros / turns
            ));
            reasons.push(format!(
                "{} failures per thousand turns",
                sample.failures.saturating_mul(1_000) / turns
            ));
        }
    }
    let state = health.probe_state(&candidate.key);
    if state.is_known() {
        reasons.push(format!("health probe reports it {state:?}"));
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_profile;
    use crate::pulse::ProbeObservations;

    fn candidate(provider: &str, model: &str) -> Candidate {
        Candidate {
            key: ModelKey {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            capabilities: model_profile::declared(None),
            residency: None,
            cost_micros_per_1k: None,
            context_window: None,
            modalities: BTreeSet::new(),
            provider_features: BTreeSet::new(),
        }
    }

    fn routed(decision: &Decision) -> &ModelKey {
        decision.key().expect("a model was chosen")
    }

    #[test]
    fn a_pin_is_filtered_like_anything_else() {
        let candidates = [candidate("acme", "fast"), candidate("other", "slow")];
        let pinned = candidates[1].key.clone();
        let constraints = Constraints {
            allowed_providers: Some(["acme".to_owned()].into()),
            ..Constraints::default()
        };
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                pinned: Some(pinned),
                route: true,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. }
                if reason.contains("provider.allowed")),
            "{decision:?}"
        );

        // Pinning something the ceiling allows is honoured, and says so.
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                pinned: Some(candidates[0].key.clone()),
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "fast");
        assert!(matches!(decision, Decision::Chosen { why, .. } if why.contains("pinned")));
    }

    #[test]
    fn disabling_routing_still_applies_policy() {
        let candidates = [candidate("acme", "fast"), candidate("other", "slow")];
        let constraints = Constraints {
            allowed_models: Some(["fast".to_owned()].into()),
            ..Constraints::default()
        };
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                default: Some(candidates[1].key.clone()),
                route: false,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. } if reason.contains("not allowed")),
            "{decision:?}"
        );
        assert_eq!(decision.excluded().len(), 1);
        assert!(decision.excluded()[0].reason.contains("model.allowed"));

        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                default: Some(candidates[0].key.clone()),
                route: false,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "fast");
    }

    #[test]
    fn residency_and_capability_ceilings_exclude_with_a_reason() {
        let mut regional = candidate("acme", "eu");
        regional.residency = Some("eu".to_owned());
        let mut elsewhere = candidate("acme", "us");
        elsewhere.residency = Some("us".to_owned());
        let unstated = candidate("acme", "unknown");
        let mut incapable = candidate("acme", "plain");
        incapable.residency = Some("eu".to_owned());
        incapable.capabilities.remove("tool_calls");

        let decision = decide(
            &[
                regional.clone(),
                elsewhere.clone(),
                unstated.clone(),
                incapable.clone(),
            ],
            &Constraints {
                residency: Some(["eu".to_owned()].into()),
                required_capabilities: ["tool_calls".to_owned()].into(),
                ..Constraints::default()
            },
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "eu");
        let excluded: BTreeMap<_, _> = decision
            .excluded()
            .iter()
            .map(|entry| (entry.key.model.clone(), entry.reason.clone()))
            .collect();
        assert!(excluded["us"].contains("not allowed"));
        assert!(excluded["unknown"].contains("unstated"));
        assert!(excluded["plain"].contains("tool_calls"));
    }

    #[test]
    fn role_requirements_filter_before_ranking_and_observations_keep_provenance() {
        let mut eligible = candidate("acme", "eligible");
        eligible.context_window = Some(128_000);
        eligible.modalities.insert("image".into());
        eligible.provider_features.insert("json_schema".into());
        let too_small = candidate("acme", "small");
        let constraints = Constraints {
            min_context_tokens: Some(64_000),
            required_modalities: ["image".to_owned()].into(),
            required_provider_features: ["json_schema".to_owned()].into(),
            ..Constraints::default()
        };
        let decision = decide(
            &[too_small, eligible.clone()],
            &constraints,
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                role: Some(AgentRole::Reviewer),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision), &eligible.key);
        assert!(decision.excluded()[0].reason.contains("context window"));

        let mut observations = Observations::new();
        observations.record_observation(Observation {
            key: eligible.key,
            observed_at_ms: 42,
            endpoint_version: "v1".into(),
            sample_size: 3,
            eval_suite: "held-out-review".into(),
            role: Some(AgentRole::Reviewer),
            task_class: TaskClass::Batch,
            latency_ms: 100,
            cost_micros: 20,
            verified_success: true,
        });
        assert_eq!(observations.turns(&observations.records()[0].key), 3);
        assert_eq!(observations.records()[0].eval_suite, "held-out-review");
    }

    #[test]
    fn ranking_uses_only_observations_for_the_requested_role_and_task_class() {
        let candidates = vec![candidate("acme", "planner"), candidate("acme", "coder")];
        let mut observations = Observations::new();
        for (key, role, latency) in [
            (&candidates[0].key, AgentRole::Planner, 10),
            (&candidates[1].key, AgentRole::Planner, 100),
            (&candidates[0].key, AgentRole::Implementer, 100),
            (&candidates[1].key, AgentRole::Implementer, 10),
        ] {
            observations.record_observation(Observation {
                key: key.clone(),
                observed_at_ms: 1,
                endpoint_version: "v1".into(),
                sample_size: 10,
                eval_suite: "held-out-role".into(),
                role: Some(role),
                task_class: TaskClass::Interactive,
                latency_ms: latency,
                cost_micros: 1,
                verified_success: true,
            });
        }

        let routed_for = |role| {
            decide(
                &candidates,
                &Constraints::default(),
                &observations,
                &ProbeObservations::new(),
                &Preference {
                    route: true,
                    role: Some(role),
                    ..Preference::default()
                },
            )
        };
        assert_eq!(routed(&routed_for(AgentRole::Planner)).model, "planner");
        assert_eq!(routed(&routed_for(AgentRole::Implementer)).model, "coder");
    }

    #[test]
    fn measurement_decides_and_the_decision_says_which() {
        let candidates = [candidate("acme", "quick"), candidate("acme", "cheap")];
        let mut observations = Observations::new();
        for _ in 0..10 {
            observations.record(&candidates[0].key, 200, 900, true);
            observations.record(&candidates[1].key, 2_000, 100, true);
        }

        let interactive = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &ProbeObservations::new(),
            &Preference {
                route: true,
                task: Some(TaskClass::Interactive),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&interactive).model, "quick");
        let Decision::Routed { reasons, .. } = &interactive else {
            panic!("routed");
        };
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("mean latency 200 ms")));

        let batch = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &ProbeObservations::new(),
            &Preference {
                route: true,
                task: Some(TaskClass::Batch),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&batch).model, "cheap");

        // Failures outrank both: a model that fails is not fast or cheap.
        for _ in 0..5 {
            observations.record(&candidates[0].key, 200, 900, false);
        }
        let interactive = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &ProbeObservations::new(),
            &Preference {
                route: true,
                task: Some(TaskClass::Interactive),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&interactive).model, "cheap");
    }

    #[test]
    fn an_unmeasured_model_does_not_win_on_a_missing_number() {
        let measured = candidate("acme", "measured");
        let unmeasured = candidate("acme", "unmeasured");
        let mut observations = Observations::new();
        observations.record(&measured.key, 5_000, 5_000, true);

        let decision = decide(
            &[unmeasured, measured.clone()],
            &Constraints::default(),
            &observations,
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "measured");

        // With nothing measured at all the choice is still deterministic, and
        // it says plainly that it had no measurements.
        let decision = decide(
            &[candidate("b", "second"), candidate("a", "first")],
            &Constraints::default(),
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).provider, "a");
        let Decision::Routed { reasons, .. } = &decision else {
            panic!("routed");
        };
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("nothing has been measured")));
    }

    #[test]
    fn a_cost_cap_and_an_empty_field_are_both_refusals_with_reasons() {
        let mut expensive = candidate("acme", "expensive");
        expensive.cost_micros_per_1k = Some(10_000);
        let decision = decide(
            &[expensive],
            &Constraints {
                max_cost_micros: Some(1_000),
                ..Constraints::default()
            },
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. }
                if reason.contains("no configured model")),
            "{decision:?}"
        );
        assert!(decision.excluded()[0]
            .reason
            .contains("exceeds the 1000 cap"));

        let decision = decide(
            &[],
            &Constraints::default(),
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference::default(),
        );
        assert!(matches!(&decision, Decision::Refused { reason, .. }
            if reason.contains("no default model")));
    }

    /// The health-probe penalty breaks a tie only, never a measured record.
    #[test]
    fn health_penalty_breaks_a_tie_but_never_a_measured_record() {
        // Two candidates with identical (empty) measurements: the healthy one wins.
        let candidates = [candidate("acme", "down"), candidate("acme", "up")];
        let mut health = ProbeObservations::new();
        health.observe(
            candidates[0].key.clone(),
            crate::pulse::ProbeState::Unreachable,
            1,
            None,
        );
        health.observe(
            candidates[1].key.clone(),
            crate::pulse::ProbeState::Healthy,
            1,
            None,
        );
        let decision = decide(
            &candidates,
            &Constraints::default(),
            &Observations::new(),
            &health,
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "up", "healthy model wins the tie");
        let Decision::Routed { reasons, .. } = &decision else {
            panic!("routed");
        };
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("health probe reports")));

        // A measured record still outranks the probe: the healthy-but-slow
        // model with real turns beats the unmeasured one.
        let mut observations = Observations::new();
        for _ in 0..10 {
            observations.record(&candidates[1].key, 1_000, 1_000, true);
        }
        // Flip: now "down" is healthy and has no turns; "up" is unreachable
        // but measured. The measured record must win.
        let mut health2 = ProbeObservations::new();
        health2.observe(
            candidates[0].key.clone(),
            crate::pulse::ProbeState::Healthy,
            1,
            None,
        );
        health2.observe(
            candidates[1].key.clone(),
            crate::pulse::ProbeState::Unreachable,
            1,
            None,
        );
        let decision = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &health2,
            &Preference {
                route: true,
                task: Some(TaskClass::Interactive),
                ..Preference::default()
            },
        );
        // "up" is measured and cheap (recorded), despite being unreachable by
        // probe; the measured record leads.
        assert_eq!(routed(&decision).model, "up");
    }

    /// An allowlist a layer wrote as empty forbids everything. Reading it as
    /// "no ceiling" routed to an endpoint the configuration had capped out,
    /// and the refusal then blamed the endpoint for not existing.
    #[test]
    fn an_empty_allowlist_is_a_ceiling_that_admits_nothing() {
        let candidates = [candidate("acme", "fast")];

        let capped = decide(
            &candidates,
            &Constraints {
                allowed_providers: Some(BTreeSet::new()),
                ..Constraints::default()
            },
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert!(capped.key().is_none(), "an empty allowlist admits nothing");
        let Decision::Refused { excluded, .. } = &capped else {
            panic!("a capped-out set is a refusal, not a route");
        };
        assert_eq!(
            excluded[0].reason, "provider.allowed does not include it",
            "the refusal names the ceiling that did it"
        );

        // Unset is the opposite answer, and still routes.
        let open = decide(
            &candidates,
            &Constraints::default(),
            &Observations::new(),
            &ProbeObservations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert!(open.key().is_some(), "no ceiling is not an empty ceiling");
    }
}
