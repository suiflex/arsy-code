//! Subagents: children of a task, holding less authority than it does.
//!
//! # Why a child is not just another prompt
//!
//! "Spawn a subagent" usually means sending the model a second conversation
//! and pasting the answer back. That is a prompt wrapper: the child can do
//! everything the parent could, nothing recorded it, and an interruption loses
//! it. Here a child is a node in the same durable task graph, with its own
//! lease, its own budget taken out of the parent's, and its own capability
//! grants attenuated from the parent's — narrower scope, shorter expiry, one
//! less delegation left.
//!
//! # Writer isolation, and why it is a consequence rather than a feature
//!
//! A child's grants become the rule set its tool runtime evaluates against. A
//! child that asked to read and search therefore *cannot* write, whatever
//! policy would have allowed the parent — not because a flag says
//! `ReadOnlySnapshot`, but because no rule in its runtime permits `fs.write`.
//! One writer per workspace falls out of that: the parent is the only task
//! that ever asked for write authority.
//!
//! # What the observer is for
//!
//! The supervisor watches each child through redacted projections of what the
//! child's runtime did — tool names and outcomes, never arguments or file
//! contents. It may suggest, and, when its authority allows, deny: a child
//! that spends its round failing is stopped rather than left to burn the
//! budget it was given. Every intervention is recorded with what it cost.

use crate::{provider, Emitter};
use arsy_code::agent::{ExecutionMode, ToolResult, ToolRuntime};
use arsy_kernel::{
    capability::{CapabilityAction, CapabilityGrant, ResourcePattern, ResourceScope},
    config::Config,
    domain::{AgentId, Principal, SubscriptionId, TaskId},
    observer::{Intervention, ObserverAuthority, ObserverSubscription, RedactedProjection},
    orchestration::{Budget, ChildCapabilityRequest, TaskGraph, TaskNode, WorkspaceRequirement},
    policy::{ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance},
    protocol::IdempotencyKey,
    provider::{
        CanonicalModelRequest, ModelContent, ModelKey, ModelMessage, ModelRole, ToolSchema,
    },
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::PathBuf};

/// The most children one turn may spawn.
///
/// A model that can spawn without bound spawns without bound. Low enough that
/// a runaway is a nuisance rather than a bill.
pub const MAX_CHILDREN: usize = 4;

/// The share of the parent's remaining budget one child may take.
///
/// A quarter, so four children fit and the parent keeps something to read
/// their answers with.
const CHILD_BUDGET_SHARE: u64 = 4;

/// What a child may ask for. Read-only by name: writing is the parent's job,
/// and a list is what makes that reviewable rather than implied.
const DELEGABLE: &[(&str, CapabilityAction)] = &[
    ("fs.read", CapabilityAction::FsRead),
    ("process.exec", CapabilityAction::ProcessExec),
];

/// How many failures in a row mean a child is not working.
///
/// Three: one is ordinary, two can be a correction, and a third in a row is a
/// child repeating itself with the rounds someone else is paying for.
const CONSECUTIVE_FAILURES: u64 = 3;

/// What an observer may spend on one child, in micro-units of its own budget.
/// Each intervention costs one; the bound is how many times it may act before
/// it has to stop watching.
const OBSERVER_BUDGET: u64 = 16;

/// How many children one turn may still start.
///
/// A type rather than a counter beside a check, because the two have to move
/// together: the bug this replaces was a check with no increment anywhere, and
/// nothing about a bare `usize` made that visible.
#[derive(Debug, Default)]
struct Allowance {
    started: usize,
}

impl Allowance {
    /// Spend a slot, or say why there is none. Counts the attempt, not the
    /// outcome.
    fn take(&mut self) -> Result<(), String> {
        if self.started >= MAX_CHILDREN {
            return Err(format!(
                "this turn has already spawned {MAX_CHILDREN} subagents; do the rest yourself"
            ));
        }
        self.started += 1;
        Ok(())
    }
}

/// The tool a supervisor offers on top of the workspace tools.
///
/// Not a workspace operation: spawning changes no file and runs no command, it
/// adds a node to the session's own graph. The child's effects each go through
/// the one path a tool call takes, under the child's own grants.
pub fn schema() -> ToolSchema {
    ToolSchema {
        name: "task.spawn".to_owned(),
        description: format!(
            "Delegate a self-contained question to a subagent that can read and search but \
             cannot write. The current turn waits for the answer, so use it for bounded \
             investigations such as \"where is X configured\" or \"what calls Y\", and not for \
             anything that changes a file. At most {MAX_CHILDREN} per turn."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "What the subagent should find out, stated so its answer is useful on its own."
                },
                "capabilities": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["fs.read", "process.exec"]},
                    "description": "What it may do. Defaults to `fs.read`. Never includes writing."
                }
            },
            "required": ["goal"],
            "additionalProperties": false,
        }),
    }
}

/// One turn's authority to create and run children.
pub struct Supervisor<'a> {
    root: PathBuf,
    config: &'a Config,
    resolved: &'a provider::Resolved,
    model: String,
    parent: TaskId,
    /// What the parent may hand on. Empty when policy grants nothing that may
    /// be delegated, which is the default and refuses every spawn.
    delegable: Vec<CapabilityGrant>,
    /// The parent's execution ceiling. A child runs under its own runtime, so
    /// without this a supervisor in Plan Mode could delegate the write it is
    /// itself refused — authority is attenuated by delegation, never widened.
    mode: ExecutionMode,
    observer: ObserverSubscription,
    allowance: Allowance,
    /// What the observer did, for the turn's record.
    interventions: Vec<Value>,
}

impl<'a> Supervisor<'a> {
    pub fn new(
        root: PathBuf,
        config: &'a Config,
        resolved: &'a provider::Resolved,
        model: String,
        parent: TaskId,
        runtime: &ToolRuntime,
    ) -> Self {
        let delegable = runtime.delegable_grants(
            &DELEGABLE
                .iter()
                .map(|(_, action)| *action)
                .collect::<Vec<_>>(),
        );
        Self {
            root,
            config,
            resolved,
            model,
            parent,
            delegable,
            mode: runtime.execution_mode(),
            observer: ObserverSubscription {
                id: SubscriptionId::new(),
                observer: AgentId::new(),
                authority: ObserverAuthority {
                    // The supervisor may stop a child it is paying for. It may
                    // not do anything else: an observer that could act would be
                    // an agent, and this one has no tools.
                    may_suggest: true,
                    may_deny: true,
                },
                cost_budget_micros: OBSERVER_BUDGET,
                cost_used_micros: 0,
            },
            allowance: Allowance::default(),
            interventions: Vec::new(),
        }
    }

    /// Whether this turn should be offered the spawn tool at all.
    ///
    /// A workspace whose policy delegates nothing gets no spawn tool rather
    /// than a tool that always refuses: an offered tool the model cannot use
    /// costs a round to discover.
    pub fn can_delegate(&self) -> bool {
        !self.delegable.is_empty()
    }

    pub fn interventions(&self) -> &[Value] {
        &self.interventions
    }

    /// Run one `task.spawn` call to the child's answer.
    pub fn spawn(
        &mut self,
        arguments: &Value,
        graph: &mut TaskGraph,
        emitter: &mut Emitter,
    ) -> ToolResult {
        let started = std::time::Instant::now();
        // Taken before anything can go wrong, so a spawn that fails still
        // spends its slot: a bound that only counted the children that worked
        // would let a model spawn failures without end, and each one costs the
        // rounds a child is allowed.
        if let Err(refusal) = self.allowance.take() {
            return ToolResult::refused("task.spawn", refusal);
        }
        let goal = arguments
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if goal.is_empty() {
            return ToolResult::refused("task.spawn", "a subagent needs a goal to work towards");
        }
        let asked = match requested(&self.delegable, arguments) {
            Ok(asked) => asked,
            Err(reason) => return ToolResult::refused("task.spawn", reason),
        };

        match self.run_child(&goal, &asked, graph, emitter) {
            Ok(answer) => ToolResult {
                tool: "task.spawn".to_owned(),
                success: true,
                output: answer,
                changed_files: Vec::new(),
                duration: started.elapsed(),
                metadata: Value::Null,
                artifact: None,
            },
            Err(reason) => ToolResult::refused("task.spawn", reason),
        }
    }
}

/// The actions asked for, checked against what may be delegated at all.
fn requested(
    delegable: &[CapabilityGrant],
    arguments: &Value,
) -> Result<Vec<CapabilityAction>, String> {
    {
        let named: Vec<&str> = arguments
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|listed| listed.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| vec!["fs.read"]);
        if named.is_empty() {
            return Err("a subagent with no capability can do nothing; ask for `fs.read`".into());
        }
        let mut actions = BTreeSet::new();
        for name in named {
            let Some((_, action)) = DELEGABLE.iter().find(|(known, _)| *known == name) else {
                return Err(format!(
                    "`{name}` cannot be delegated; a subagent may ask for {}",
                    DELEGABLE
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            };
            if !delegable.iter().any(|grant| grant.action == *action) {
                return Err(format!(
                    "this workspace does not delegate `{name}`; a rule must grant it with a \
                     delegation depth above zero before a subagent can hold it"
                ));
            }
            actions.insert(*action);
        }
        Ok(actions.into_iter().collect())
    }
}

impl Supervisor<'_> {
    /// Record the child, attenuate its authority, run it, and close it.
    fn run_child(
        &mut self,
        goal: &str,
        actions: &[CapabilityAction],
        graph: &mut TaskGraph,
        emitter: &mut Emitter,
    ) -> Result<String, String> {
        let agent = AgentId::new();
        let id = TaskId::new();
        let parent_budget = graph
            .node(self.parent)
            .map(|node| node.budget)
            .ok_or_else(|| "the parent task is not in its own graph".to_owned())?;
        // The parent's authority is written to the graph the first time it
        // delegates: a child's grants have to be explicable from the record,
        // and `add_child` attenuates from what the graph holds rather than
        // from whatever this process happens to be carrying.
        if graph
            .node(self.parent)
            .is_some_and(|node| node.authority.is_empty())
        {
            graph
                .authorize(self.parent, self.delegable.clone())
                .map_err(|error| {
                    format!("the parent's authority could not be recorded: {error}")
                })?;
        }
        let requests: Vec<ChildCapabilityRequest> = actions
            .iter()
            .filter_map(|action| {
                let index = self
                    .delegable
                    .iter()
                    .position(|grant| grant.action == *action)?;
                Some(ChildCapabilityRequest {
                    parent_grant: index,
                    action: *action,
                    // The workspace, and no wider: a child inherits the
                    // parent's scope narrowed by this, never widened.
                    scope: ResourceScope::single(
                        ResourcePattern::new(action.default_scheme(), "**").ok()?,
                    ),
                    expires_at_ms: self.delegable[index].expires_at_ms,
                })
            })
            .collect();
        if requests.len() != actions.len() {
            return Err("a requested capability could not be attenuated from the parent".into());
        }

        graph
            .add_child(
                self.parent,
                TaskNode {
                    id,
                    goal: goal.to_owned(),
                    dependencies: Vec::new(),
                    assignee: Some(agent),
                    required_output: "an answer the parent can act on".to_owned(),
                    // The child reads a workspace someone else may be writing.
                    workspace: WorkspaceRequirement::ReadOnlySnapshot,
                    budget: share(parent_budget),
                    authority: Vec::new(),
                    state: arsy_kernel::orchestration::TaskState::Pending,
                    lease_expires_at_ms: None,
                    runtime: Default::default(),
                },
                requests,
            )
            .map_err(|error| format!("the subagent could not be recorded: {error}"))?;
        graph
            .ready()
            .and_then(|_| {
                graph.lease(
                    id,
                    agent,
                    arsy_kernel::artifact::unix_time_ms() + share(parent_budget).wall_ms,
                )
            })
            .map_err(|error| format!("the subagent could not be started: {error}"))?;

        let granted = graph
            .node(id)
            .map(|node| node.authority.clone())
            .unwrap_or_default();
        let outcome = self.execute(goal, &granted, agent, emitter);
        match &outcome {
            Ok(answer) => {
                let _ = graph.complete(id, json!({"answer_bytes": answer.len()}));
            }
            Err(reason) => {
                let _ = graph.fail(id, json!({"message": reason}));
            }
        }
        outcome
    }

    /// One child turn, under a runtime that can do only what the child holds.
    fn execute(
        &mut self,
        goal: &str,
        granted: &[CapabilityGrant],
        agent: AgentId,
        emitter: &mut Emitter,
    ) -> Result<String, String> {
        let workspace =
            arsy_code::resource::Workspace::open(&self.root).map_err(|error| error.to_string())?;
        let artifacts = std::sync::Arc::new(
            arsy_kernel::artifact::FileArtifactStore::open(self.root.join(".arsy/artifacts"), 0)
                .map_err(|error| error.to_string())?,
        );
        let runtime = arsy_code::agent::runtime(
            &workspace,
            // The child's grants, as rules. Expressing them in the engine's own
            // vocabulary means the child is authorized by the same code path
            // the parent is, rather than by a second implementation that could
            // disagree with it.
            rules_from(granted),
            artifacts,
            arsy_kernel::artifact::unix_time_ms(),
            Principal::Agent(agent),
            RiskContext {
                reversible: false,
                workspace: arsy_code::git::cleanliness(&self.root)
                    .unwrap_or(arsy_kernel::policy::WorkspaceCleanliness::Unknown),
                sandbox: crate::installed_sandbox_assurance(),
            },
            arsy_code::operations::Reachable::from_config(self.config),
            // The child's own plan and validation history, not the parent's:
            // a delegated subtask should not inherit or pollute the plan the
            // supervisor is tracking.
            &agent.to_string(),
            // A delegate reports back to its parent; the parent owns the
            // session's checklist, so a child does not get one of its own.
            arsy_code::operations::TurnState::default(),
        )
        .map_err(|error| error.to_string())?
        .with_execution_mode(self.mode);

        let request = CanonicalModelRequest {
            model: ModelKey {
                provider: self.resolved.endpoint.id.clone(),
                model: self.model.clone(),
            },
            system: Some(child_instructions(goal)),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: goal.to_owned(),
                }],
            }],
            tools: runtime.schemas(),
            max_output_tokens: self.resolved.endpoint.max_output_tokens,
            effort: None,
            idempotency_key: IdempotencyKey::new(agent.to_string())
                .map_err(|error| error.to_string())?,
        };

        crate::child_turn(
            self.resolved.provider.as_ref(),
            &runtime,
            &request,
            &mut |projection| self.watch(projection),
            emitter,
        )
    }

    /// Offer one redacted projection of the child's activity to the observer.
    ///
    /// Returns the intervention the observer made, if any. A `Deny` stops the
    /// child; a `Suggest` is recorded and reaches the parent with the answer.
    fn watch(&mut self, projection: &RedactedProjection) -> Option<Intervention> {
        let intervention = warranted(projection)?;
        match self.observer.intervene(projection, intervention.clone(), 1) {
            Ok(event) => {
                self.interventions.push(json!({
                    "subscription": event.subscription.to_string(),
                    "at_sequence": event.projection_sequence,
                    "intervention": event.intervention,
                    "cost_micros": event.cost_micros,
                }));
                Some(intervention)
            }
            // Out of budget or out of authority: the observer stops observing
            // rather than acting beyond what it was given.
            Err(_) => None,
        }
    }
}

/// What this projection warrants, if anything.
///
/// The rule is small and explainable on purpose: a child whose calls keep
/// failing is not working, and stopping it is worth more than the rounds it
/// would spend proving that.
///
/// The count comes from the payload, not from the sequence: the sequence is
/// which call this was, and an intervention that recorded a failure count in
/// its place would be a false entry in the audit.
fn warranted(projection: &RedactedProjection) -> Option<Intervention> {
    let failures = projection
        .public_payload
        .get("consecutive_failures")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (projection.kind == "tool.failed" && failures >= CONSECUTIVE_FAILURES)
        .then(|| Intervention::Deny(format!("{failures} tool calls in a row failed")))
}

/// A quarter of what is left, and at least enough to be worth starting.
fn share(parent: Budget) -> Budget {
    Budget {
        tokens: parent.tokens / CHILD_BUDGET_SHARE,
        cost_micros: parent.cost_micros / CHILD_BUDGET_SHARE,
        wall_ms: parent.wall_ms / CHILD_BUDGET_SHARE,
    }
}

/// Grants as the rules that admit exactly them.
fn rules_from(granted: &[CapabilityGrant]) -> RuleSet {
    RuleSet::compile(granted.iter().flat_map(|grant| {
        grant
            .scope
            .patterns()
            .iter()
            .map(|pattern| PolicyRule {
                // The parent's own authority is what this came from, so it
                // carries the parent's source rather than claiming more.
                source: grant.source,
                effect: RuleEffect::Allow,
                actor: ActorMatch::Exactly(grant.actor.clone()),
                action: grant.action,
                pattern: pattern.clone(),
                expires_at_ms: grant.expires_at_ms,
                delegation_depth: grant.delegation_depth,
                minimum_assurance: SandboxAssurance::None,
            })
            .collect::<Vec<_>>()
    }))
}

/// What a child is told about its own position.
fn child_instructions(goal: &str) -> String {
    format!(
        "You are a subagent with one question to answer: {goal}\n\n\
         You can read and search this workspace. You cannot write to it, and asking to will be \
         refused — the agent that spawned you owns every change. Answer in a few sentences, \
         citing the paths you read. If the answer is not in this workspace, say so rather than \
         guessing.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_may_ask_only_for_what_can_be_delegated() {
        use arsy_kernel::capability::PolicySource;
        let grant = |action: CapabilityAction| CapabilityGrant {
            id: arsy_kernel::domain::GrantId::new(),
            actor: Principal::User("dev".into()),
            action,
            scope: ResourceScope::single(
                ResourcePattern::new(action.default_scheme(), "**").unwrap(),
            ),
            expires_at_ms: None,
            delegation_depth: 1,
            source: PolicySource::User,
        };
        let supervisor = |delegable: Vec<CapabilityGrant>| Requested { delegable };

        // Writing is not on the list at all, whatever the parent holds.
        let all = supervisor(vec![
            grant(CapabilityAction::FsRead),
            grant(CapabilityAction::FsWrite),
        ]);
        assert!(all
            .requested(&json!({"capabilities": ["fs.write"]}))
            .unwrap_err()
            .contains("cannot be delegated"));

        // On the list, but this workspace delegates nothing.
        let none = supervisor(Vec::new());
        assert!(none
            .requested(&json!({"capabilities": ["fs.read"]}))
            .unwrap_err()
            .contains("does not delegate"));

        // The default is the narrowest thing that is useful.
        let reader = supervisor(vec![grant(CapabilityAction::FsRead)]);
        assert_eq!(
            reader.requested(&json!({})).unwrap(),
            vec![CapabilityAction::FsRead]
        );
        assert!(reader
            .requested(&json!({"capabilities": []}))
            .unwrap_err()
            .contains("no capability"));
    }

    #[test]
    fn a_childs_rules_admit_its_grants_and_nothing_else() {
        let agent = AgentId::new();
        let read = CapabilityGrant {
            id: arsy_kernel::domain::GrantId::new(),
            actor: Principal::Agent(agent),
            action: CapabilityAction::FsRead,
            scope: ResourceScope::single(ResourcePattern::new("file", "**").unwrap()),
            expires_at_ms: None,
            delegation_depth: 0,
            source: arsy_kernel::capability::PolicySource::User,
        };
        let rules = rules_from(&[read]);

        let query = |action: CapabilityAction, actor: Principal| arsy_kernel::policy::PolicyQuery {
            actor,
            operation: arsy_kernel::operation::OperationKind::new("fs.read").unwrap(),
            requirement: arsy_kernel::capability::CapabilityRequirement {
                action,
                resource: arsy_kernel::domain::ResourceRef::new(
                    action.default_scheme(),
                    "src/lib.rs",
                )
                .unwrap(),
            },
            operation_digest: arsy_kernel::domain::StateVersion::from_digest([0; 32]),
            resource_version: None,
            context: RiskContext {
                reversible: true,
                workspace: arsy_kernel::policy::WorkspaceCleanliness::Clean,
                sandbox: SandboxAssurance::None,
            },
        };

        assert!(rules
            .evaluate(&query(CapabilityAction::FsRead, Principal::Agent(agent)))
            .decision
            .is_allow());
        // Writing was never granted, so the child's own runtime refuses it.
        assert!(!rules
            .evaluate(&query(CapabilityAction::FsWrite, Principal::Agent(agent)))
            .decision
            .is_allow());
        // And the grant is the child's, not anyone else's.
        assert!(!rules
            .evaluate(&query(
                CapabilityAction::FsRead,
                Principal::User("dev".into())
            ))
            .decision
            .is_allow());
    }

    #[test]
    fn a_child_budget_is_a_share_of_what_the_parent_has_left() {
        let parent = Budget {
            tokens: 400,
            cost_micros: 40,
            wall_ms: 4_000,
        };
        let child = share(parent);
        assert_eq!(child.tokens, 100);
        assert!(
            child.fits_within(parent),
            "a child never exceeds its parent"
        );
    }

    /// A projection as `child_turn` builds one.
    fn projection(call: u64, failures: u64, kind: &str) -> RedactedProjection {
        RedactedProjection {
            sequence: call,
            kind: kind.to_owned(),
            public_payload: json!({"tool": "fs.read", "consecutive_failures": failures}),
            redacted_fields: 2,
        }
    }

    #[test]
    fn the_sequence_is_which_call_it_was_and_the_count_lives_in_the_payload() {
        // `warranted` is the rule the supervisor runs, called here rather than
        // copied: a test that reimplements the rule passes against a
        // supervisor that reads the wrong field.
        assert!(
            warranted(&projection(10, 2, "tool.failed")).is_none(),
            "two in a row is not yet a pattern"
        );
        assert!(
            warranted(&projection(11, 0, "tool.completed")).is_none(),
            "a success is never an intervention"
        );

        match warranted(&projection(12, 3, "tool.failed")) {
            // The reason names the count, not the call number: the two were
            // once the same field, and that made the audit trail wrong.
            Some(Intervention::Deny(reason)) => assert!(reason.starts_with("3 "), "{reason}"),
            other => panic!("three in a row is denied, not {other:?}"),
        }
    }

    #[test]
    fn an_observer_stops_a_child_that_keeps_failing_and_stops_when_out_of_budget() {
        let mut observer = ObserverSubscription {
            id: SubscriptionId::new(),
            observer: AgentId::new(),
            authority: ObserverAuthority {
                may_suggest: true,
                may_deny: true,
            },
            cost_budget_micros: 1,
            cost_used_micros: 0,
        };
        let projection = RedactedProjection {
            sequence: 3,
            kind: "tool.failed".to_owned(),
            public_payload: json!({"tool": "fs.read"}),
            redacted_fields: 1,
        };

        assert!(observer
            .intervene(&projection, Intervention::Deny("failing".into()), 1)
            .is_ok());
        // The budget is spent, so it observes without acting rather than
        // acting without a budget.
        assert!(observer
            .intervene(&projection, Intervention::Deny("failing".into()), 1)
            .is_err());
    }

    /// The half of [`Supervisor`] these tests exercise, without a provider or a
    /// workspace behind it.
    struct Requested {
        delegable: Vec<CapabilityGrant>,
    }

    impl Requested {
        fn requested(&self, arguments: &Value) -> Result<Vec<CapabilityAction>, String> {
            super::requested(&self.delegable, arguments)
        }
    }

    #[test]
    fn every_spawn_counts_against_the_bound_including_the_ones_that_fail() {
        // Driving the real type, not a copy of its logic: the bug this covers
        // was a bound that was checked and never incremented, and a test that
        // counted for itself would have passed against it.
        let mut allowance = Allowance::default();
        for _ in 0..MAX_CHILDREN {
            allowance.take().expect("the allowance is not spent yet");
        }
        let refused = allowance
            .take()
            .expect_err("the allowance is spent, whatever the children did with it");
        assert!(refused.contains("already spawned"), "{refused}");
    }
}
