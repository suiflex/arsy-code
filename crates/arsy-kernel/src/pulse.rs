//! Model-health pulse: the sync port routing consults, kept apart from turns.
//!
//! See `docs/09-model-provider-layer.md`. A health probe is **not** a turn:
//! a failed probe is not a failed turn. So probe state lives in its own store
//! ([`ProbeObservations`]) and never in [`crate::routing::Observations`], which
//! derives `mean_latency_ms` and `failure_rate` from turns. The probe result
//! is a *tie-break* penalty in routing, never a primary criterion — measured
//! turn records always lead.
//!
//! The kernel defines the [`HealthProbe`] port and the state model; the CLI
//! fills the port from the bundled `probelm` MCP connection. This module is
//! pure and synchronous — no tokio, no HTTP — so the kernel stays runtime-free.

use crate::provider::ModelKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Health state of a model endpoint, as reported by an external health
/// channel. Values serialize snake_case.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    /// No data yet, or the channel is silent about this model.
    #[default]
    Unknown,
    Healthy,
    /// Reachable but degraded (e.g. no tokens streamed).
    Degraded,
    /// Not reachable or refused the request.
    Unreachable,
}

impl ProbeState {
    /// Tie-break penalty for routing. An unreachable model loses a tie to a
    /// reachable one, but never overrides a measured turn record. `Unknown`
    /// does not penalize because no data should not look like a verdict.
    pub fn penalty(self) -> u8 {
        match self {
            ProbeState::Unreachable => 2,
            ProbeState::Degraded => 1,
            ProbeState::Healthy | ProbeState::Unknown => 0,
        }
    }

    /// True when the channel has said something about this model.
    pub fn is_known(self) -> bool {
        self != ProbeState::Unknown
    }
}

/// What the pulse knows about one model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeRecord {
    pub state: ProbeState,
    /// Unix epoch ms of the most recent probe that produced `state`.
    pub checked_at_ms: Option<u64>,
    /// Number of state changes observed (the durable event count proxy).
    pub transitions: u64,
    /// Last per-probe error text, when the model is not healthy.
    pub last_error: Option<String>,
}

/// Sync port for model health. The kernel defines it; the CLI implements it
/// from the `probelm` MCP connection. Defaults are safe so a consumer that
/// has no health channel behaves as if nothing were known.
pub trait HealthProbe {
    /// Latest known state, or [`ProbeState::Unknown`] when there is none.
    fn probe_state(&self, _key: &ModelKey) -> ProbeState {
        ProbeState::Unknown
    }

    /// Latest known record, when the channel has one.
    fn probe_record(&self, _key: &ModelKey) -> Option<ProbeRecord> {
        None
    }
}

/// In-memory health store filled by the CLI.
///
/// This is both the *projection* routing reads and a [`HealthProbe`]
/// implementation, so the CLI can hand one object to both the router and the
/// event recorder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeObservations {
    records: BTreeMap<ModelKey, ProbeRecord>,
}

impl ProbeObservations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one observed state. When the state changed, returns the previous
    /// state so the caller can record a durable transition event (events only
    /// on state change, never per probe).
    pub fn observe(
        &mut self,
        key: ModelKey,
        state: ProbeState,
        checked_at_ms: u64,
        error: Option<String>,
    ) -> Option<ProbeState> {
        let record = self.records.entry(key).or_default();
        let previous = record.state;
        let changed = previous != state;
        record.state = state;
        record.checked_at_ms = Some(checked_at_ms);
        if changed {
            record.transitions += 1;
        }
        if let Some(error) = error {
            record.last_error = Some(error);
        }
        changed.then_some(previous)
    }

    /// Latest known state for a model.
    pub fn state(&self, key: &ModelKey) -> ProbeState {
        self.records
            .get(key)
            .map_or(ProbeState::Unknown, |record| record.state)
    }

    /// Latest known record for a model, if any.
    pub fn record(&self, key: &ModelKey) -> Option<&ProbeRecord> {
        self.records.get(key)
    }

    /// Models the channel has heard about.
    pub fn keys(&self) -> impl Iterator<Item = &ModelKey> {
        self.records.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Replace the whole view. A key held before but absent from `snapshots`
    /// goes back to Unknown, is reported as a transition, and is then dropped.
    pub fn replace_all(
        &mut self,
        snapshots: impl IntoIterator<Item = (ModelKey, ProbeState, u64, Option<String>)>,
        now_ms: u64,
    ) -> Vec<HealthChanged> {
        let mut changes = Vec::new();
        let mut incoming = std::collections::BTreeSet::new();
        for (key, state, checked_at_ms, error) in snapshots {
            if let Some(previous) = self.observe(key.clone(), state, checked_at_ms, error) {
                changes.push(HealthChanged {
                    provider: key.provider.clone(),
                    model: key.model.clone(),
                    from: previous,
                    to: state,
                    checked_at_ms,
                });
            }
            incoming.insert(key);
        }
        self.records.retain(|key, record| {
            if incoming.contains(key) {
                return true;
            }
            if record.state.is_known() {
                changes.push(HealthChanged {
                    provider: key.provider.clone(),
                    model: key.model.clone(),
                    from: record.state,
                    to: ProbeState::Unknown,
                    checked_at_ms: now_ms,
                });
            }
            false
        });
        changes
    }
}

impl HealthProbe for ProbeObservations {
    fn probe_state(&self, key: &ModelKey) -> ProbeState {
        self.state(key)
    }

    fn probe_record(&self, key: &ModelKey) -> Option<ProbeRecord> {
        self.record(key).cloned()
    }
}

/// Durable event payload recorded when a model's health state changes.
///
/// Event `kind` is [`HealthChanged::EVENT_KIND`]. Recorded only on a state
/// transition, never on every probe.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthChanged {
    pub provider: String,
    pub model: String,
    pub from: ProbeState,
    pub to: ProbeState,
    pub checked_at_ms: u64,
}

impl HealthChanged {
    /// Event kind used as the envelope `kind`.
    pub const EVENT_KIND: &'static str = "model.health_changed";

    /// The model this change is about.
    pub fn key(&self) -> ModelKey {
        ModelKey {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(provider: &str, model: &str) -> ModelKey {
        ModelKey {
            provider: provider.to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn penalty_orders_unreachable_worst_unknown_least() {
        assert_eq!(ProbeState::Unreachable.penalty(), 2);
        assert_eq!(ProbeState::Degraded.penalty(), 1);
        assert_eq!(ProbeState::Healthy.penalty(), 0);
        assert_eq!(ProbeState::Unknown.penalty(), 0);
        assert!(!ProbeState::Unknown.is_known());
        assert!(ProbeState::Healthy.is_known());
    }

    #[test]
    fn observe_reports_only_real_changes() {
        let mut obs = ProbeObservations::new();
        let k = key("acme", "fast");
        // First observation: previous is Unknown (default), so this IS a change.
        assert_eq!(
            obs.observe(k.clone(), ProbeState::Healthy, 1, None),
            Some(ProbeState::Unknown)
        );
        assert_eq!(obs.state(&k), ProbeState::Healthy);
        assert_eq!(obs.record(&k).unwrap().transitions, 1);
        // Same state again: not a change.
        assert_eq!(obs.observe(k.clone(), ProbeState::Healthy, 2, None), None);
        // Different state: reports the previous.
        assert_eq!(
            obs.observe(k.clone(), ProbeState::Unreachable, 3, Some("down".into())),
            Some(ProbeState::Healthy)
        );
        assert_eq!(obs.state(&k), ProbeState::Unreachable);
        assert_eq!(obs.record(&k).unwrap().transitions, 2);
        assert_eq!(obs.record(&k).unwrap().last_error.as_deref(), Some("down"));
    }

    #[test]
    fn first_observation_is_a_transition_from_unknown() {
        let mut obs = ProbeObservations::new();
        let k = key("acme", "fast");
        let changes = obs.replace_all([(k.clone(), ProbeState::Healthy, 10, None)], 10);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from, ProbeState::Unknown);
        assert_eq!(changes[0].to, ProbeState::Healthy);
    }

    #[test]
    fn replace_all_reports_changes_only() {
        let mut obs = ProbeObservations::new();
        let k = key("acme", "fast");
        obs.observe(k.clone(), ProbeState::Healthy, 1, None);
        // Unchanged plus one changed model.
        let changes = obs.replace_all(
            [
                (k.clone(), ProbeState::Healthy, 2, None),
                (key("acme", "slow"), ProbeState::Degraded, 2, None),
            ],
            2,
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].model, "slow");
        assert_eq!(changes[0].from, ProbeState::Unknown);
        assert_eq!(changes[0].to, ProbeState::Degraded);
        assert_eq!(changes[0].key(), key("acme", "slow"));
    }

    #[test]
    fn replace_all_resets_a_key_missing_from_the_new_view() {
        let mut obs = ProbeObservations::new();
        let gone = key("acme", "gone");
        obs.observe(
            gone.clone(),
            ProbeState::Unreachable,
            1,
            Some("down".into()),
        );
        let changes = obs.replace_all([(key("acme", "fast"), ProbeState::Unknown, 5, None)], 7);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key(), gone);
        assert_eq!(changes[0].from, ProbeState::Unreachable);
        assert_eq!(changes[0].to, ProbeState::Unknown);
        assert_eq!(changes[0].checked_at_ms, 7);
        assert_eq!(obs.state(&gone), ProbeState::Unknown);
        assert_eq!(obs.record(&gone), None);
    }

    #[test]
    fn health_probe_port_returns_unknown_without_data() {
        let obs = ProbeObservations::new();
        assert_eq!(obs.probe_state(&key("acme", "fast")), ProbeState::Unknown);
        assert_eq!(obs.probe_record(&key("acme", "fast")), None);
    }
}
