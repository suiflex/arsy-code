//! The registry of operations this build can dispatch.
//!
//! One place, so the contracts a dry run explains are the contracts an
//! execution would dispatch against. A kind that is not here cannot run, and
//! `arsy policy explain` says so rather than inventing a requirement.

use crate::{
    git::{GitExecutor, GitOperation},
    process::ProcessExecutor,
    remote::RemoteExecutor,
    resource::Workspace,
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityRequirement},
    domain::ResourceRef,
    operation::{OperationContract, OperationRegistry, RegistrationError},
};
use serde_json::Value;
use std::{path::Path, sync::Arc, time::Duration};

/// The concrete resources one call needs authority over.
///
/// A contract names the actions an operation may need; the resources arrive
/// with the request. This is the one place that mapping lives, so a dry run and
/// a dispatch can never disagree about what a call is asking for.
pub fn requirements(
    contract: &OperationContract,
    input: &Value,
    workspace: &Path,
) -> Vec<CapabilityRequirement> {
    contract
        .actions
        .iter()
        .map(|action| CapabilityRequirement {
            action: *action,
            resource: resource_for(*action, input, workspace),
        })
        .collect()
}

/// What an action is exercised over, read from the call's own input where the
/// input says, and from the workspace where it does not.
fn resource_for(action: CapabilityAction, input: &Value, workspace: &Path) -> ResourceRef {
    let string = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    // The scheme is the action's own, so a rule written against it matches
    // wherever the resource is built.
    let scheme = action.default_scheme();
    let value = match action {
        CapabilityAction::FsRead
        | CapabilityAction::FsWrite
        | CapabilityAction::FsDelete
        | CapabilityAction::GitRead
        | CapabilityAction::GitWrite => {
            string("path").unwrap_or_else(|| workspace.display().to_string())
        }
        CapabilityAction::ProcessExec | CapabilityAction::ProcessSignal => input
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|argv| argv.first())
            .and_then(Value::as_str)
            .map(str::to_owned)
            // A call against a background session names only its handle, so
            // the program is looked up rather than left as `*`: an operator
            // who narrowed `process.exec` to the commands they trust must get
            // the same answer for polling one as for starting it.
            .or_else(|| {
                input
                    .get("handle")
                    .and_then(Value::as_str)
                    .and_then(crate::procsession::program_for)
            })
            .unwrap_or_else(|| "*".to_owned()),
        CapabilityAction::RemoteExec => string("target").unwrap_or_else(|| "*".into()),
        // A rule is written about a host; `net.fetch` carries a URL, so the
        // host is derived from it rather than left as `*` — otherwise an
        // operator who allowed one domain would have allowed every domain.
        CapabilityAction::NetworkConnect => string("host")
            .or_else(|| {
                input
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(crate::web::host_of)
            })
            .unwrap_or_else(|| "*".into()),
        CapabilityAction::CredentialUse => string("handle").unwrap_or_else(|| "*".into()),
        CapabilityAction::PluginInvoke => string("plugin").unwrap_or_else(|| "*".into()),
        // `<server>/<tool>`: a rule can admit one server wholesale with
        // `mcp:github/**`, or exactly one of its tools.
        CapabilityAction::McpInvoke => crate::agent::mcpops::resource_path(
            &string("server").unwrap_or_else(|| "*".into()),
            &string("tool").unwrap_or_else(|| "*".into()),
        ),
        CapabilityAction::BrowserControl
        | CapabilityAction::DebugLaunch
        | CapabilityAction::DebugAttach
        | CapabilityAction::SystemModify => "*".to_owned(),
    };
    ResourceRef::new(scheme, value)
        .unwrap_or_else(|_| ResourceRef::new(scheme, "*").expect("a static scheme and value"))
}

/// Environment variables a subprocess inherits. Everything else is dropped, so
/// a credential in the operator's shell cannot reach a tool by accident.
pub const DEFAULT_ENVIRONMENT_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "TZ"];

/// How long a terminated process has to exit before it is killed.
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// What a workspace may reach beyond its own files, as configuration resolved
/// it.
///
/// One parameter rather than two lists, because they are the same decision
/// made twice: which programs and machines this workspace is allowed to
/// involve. A registry built with the default reaches nothing outside itself.
#[derive(Clone, Debug, Default)]
pub struct Reachable {
    pub remote_targets: Vec<(String, arsy_kernel::config::RemoteTarget)>,
    pub language_servers: Vec<arsy_kernel::config::LanguageServer>,
}

impl Reachable {
    /// Everything configuration says this workspace may reach.
    pub fn from_config(config: &arsy_kernel::config::Config) -> Self {
        Self {
            remote_targets: config
                .remote_targets()
                .map(|(name, target)| (name.clone(), target.clone()))
                .collect(),
            language_servers: config.language_servers().cloned().collect(),
        }
    }
}

/// What one turn brought with it, beyond the workspace.
///
/// Both fields are the same kind of thing — state that exists for a turn and
/// not for the workspace — so they travel together rather than as two more
/// positional parameters. A registry built with the default offers neither
/// kind rather than inventing somewhere to write or someone to call.
#[derive(Clone, Default)]
pub struct TurnState {
    /// The session stream durable state is written to. `None` — a dry run, a
    /// bare tool call, a test — offers no `todo.*` kind at all.
    pub journal: Option<crate::agent::todoops::Journal>,
    /// MCP servers this turn connected to. `None` offers no `mcp.call`.
    pub mcp: Option<crate::agent::mcpops::Connections>,
    /// Servers of `mcp` still connecting, whose calls wait for them.
    pub mcp_pending: crate::agent::mcpops::Pending,
}

/// Build the registry for one workspace.
///
/// `retain_until_ms` is stamped on the artifacts operations produce, so `gc`
/// knows when their evidence may be collected.
///
/// `scope` isolates the plan a registry's `plan.*` kinds hold: two registries
/// built for the same workspace but different scopes (a session id, a task id,
/// or any other value unique to the unit of work) get their own state rather
/// than one inheriting whatever the other left. A registry rebuilt with the
/// same scope — the ordinary case, once per turn of one session — gets the
/// same state back. Validation records are not scoped that way: they are
/// evidence, and belong to the session stream they are read back from.
pub fn registry(
    workspace: &Workspace,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    reachable: Reachable,
    scope: &str,
    turn: TurnState,
) -> Result<OperationRegistry, RegistrationError> {
    let mut registry = OperationRegistry::new();
    // Validation records are evidence, so they go to the session's stream when
    // there is one. A turn with no journal keeps them for the process and says
    // so: `validate.status` reports `durable: false` rather than letting
    // records that vanish on restart read like records that do not.
    let validations = match &turn.journal {
        Some(journal) => crate::agent::validateops::Validations {
            log: Arc::new(std::sync::Mutex::new(
                arsy_kernel::validation::ValidationLog::open(
                    Arc::clone(&journal.store),
                    journal.session,
                    journal.actor.clone(),
                )
                .map_err(|error| RegistrationError::Unusable(error.to_string()))?,
            )),
            task: journal.task,
            attempt: journal.attempt,
        },
        None => crate::agent::validateops::Validations::ephemeral(),
    };
    if let Some(connections) = turn.mcp {
        registry.register(crate::agent::mcpops::McpExecutor::new(
            connections,
            turn.mcp_pending.clone(),
            Arc::clone(&artifacts),
            retain_until_ms,
        ))?;
    }
    if let Some(journal) = &turn.journal {
        // A stream that cannot be read is a broken session, not a missing
        // feature, so it is reported rather than silently dropping the kinds.
        for executor in
            crate::agent::todoops::TodoExecutor::executors(journal, &artifacts, retain_until_ms)
                .map_err(|error| RegistrationError::Unusable(error.to_string()))?
        {
            registry.register(executor)?;
        }
    }
    for operation in GitOperation::ALL {
        registry.register(GitExecutor::new(
            operation,
            workspace,
            Arc::clone(&artifacts),
            retain_until_ms,
        ))?;
    }
    // The workspace operations. They are registered here rather than by the
    // agent so that `arsy policy explain`, the MCP server, and a turn all see
    // the same set: a tool the model can call is a tool an operator can reason
    // about beforehand.
    for executor in crate::agent::fsops::executors(workspace, &artifacts, retain_until_ms)
        .into_iter()
        .chain(crate::agent::searchops::executors(
            workspace,
            &artifacts,
            retain_until_ms,
        ))
        .chain(crate::agent::codeops::executors(
            workspace,
            &artifacts,
            retain_until_ms,
            reachable.language_servers,
        ))
    {
        registry.register(executor)?;
    }
    #[cfg(feature = "dap")]
    registry.register(crate::agent::debugops::DebugExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    #[cfg(feature = "wasm")]
    registry.register(crate::agent::pluginops::PluginExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    registry.register(crate::agent::patch::PatchExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    let process = |artifacts| {
        ProcessExecutor::new(
            artifacts,
            DEFAULT_ENVIRONMENT_ALLOWLIST
                .iter()
                .map(|name| (*name).to_owned()),
            DEFAULT_TERMINATION_GRACE,
            retain_until_ms,
        )
        // A shell command runs where the same turn's file tools read and write.
        .in_directory(workspace.path())
    };
    // A remote target is only reachable when configuration defined one, so a
    // workspace with none cannot dispatch `remote.exec` at all rather than
    // dispatching it to nowhere.
    if !reachable.remote_targets.is_empty() {
        registry.register(Arc::new(RemoteExecutor::new(
            reachable.remote_targets,
            process(Arc::clone(&artifacts)),
        )))?;
    }
    registry.register(crate::web::FetchExecutor::new(
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    registry.register(crate::repomap::MapExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    registry.register(crate::agent::discoveryops::DiscoveryExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    for executor in crate::agent::planops::PlanExecutor::executors(
        &crate::agent::planops::state_for(workspace.path(), scope),
        &artifacts,
        retain_until_ms,
        turn.journal.as_ref(),
    )
    .into_iter()
    .chain(crate::agent::validateops::ValidateExecutor::executors(
        &validations,
        &artifacts,
        retain_until_ms,
        workspace.path(),
    )) {
        registry.register(executor)?;
    }
    // A background command runs under the same environment allowlist and the
    // same termination grace as a foreground one: the difference between them
    // is who waits for the exit, not what the child is allowed to see.
    for executor in crate::procsession::SessionExecutor::executors(
        workspace,
        &artifacts,
        retain_until_ms,
        scope,
        DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect(),
        DEFAULT_TERMINATION_GRACE,
    ) {
        registry.register(executor)?;
    }
    registry.register(Arc::new(process(artifacts)))?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{artifact::FileArtifactStore, operation::OperationKind};

    #[test]
    fn every_registered_kind_publishes_the_actions_it_needs() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temporary.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(temporary.path().join("artifacts"), 0).unwrap());
        let registry = registry(
            &workspace,
            Arc::clone(&artifacts),
            0,
            Reachable::default(),
            "test",
            TurnState::default(),
        )
        .unwrap();

        let kinds: Vec<_> = registry.kinds().map(ToString::to_string).collect();
        // A WASM build can dispatch a plugin; a build without the feature has
        // no host to run one in and does not offer the operation at all.
        #[cfg(feature = "wasm")]
        assert!(kinds.contains(&"plugin.invoke".to_owned()));
        #[cfg(feature = "dap")]
        assert!(kinds.contains(&"debug.run".to_owned()));
        let kinds: Vec<_> = kinds
            .into_iter()
            .filter(|kind| kind != "plugin.invoke" && kind != "debug.run")
            .collect();
        assert_eq!(
            kinds,
            vec![
                "code.diagnostics".to_owned(),
                "code.explain".to_owned(),
                "code.references".to_owned(),
                "code.rename".to_owned(),
                "code.symbol".to_owned(),
                "fs.create".to_owned(),
                "fs.delete".to_owned(),
                "fs.edit".to_owned(),
                "fs.list".to_owned(),
                "fs.move".to_owned(),
                "fs.patch".to_owned(),
                "fs.read".to_owned(),
                "fs.write".to_owned(),
                "git.blame".to_owned(),
                "git.branch".to_owned(),
                "git.diff".to_owned(),
                "git.log".to_owned(),
                "git.status".to_owned(),
                "net.fetch".to_owned(),
                "plan.add".to_owned(),
                "plan.list".to_owned(),
                "plan.remove".to_owned(),
                "plan.reorder".to_owned(),
                "plan.update".to_owned(),
                "process.exec".to_owned(),
                "process.poll".to_owned(),
                "process.start".to_owned(),
                "process.stop".to_owned(),
                "process.write".to_owned(),
                "repo.discover".to_owned(),
                "repo.map".to_owned(),
                "search.files".to_owned(),
                "search.text".to_owned(),
                "validate.record".to_owned(),
                "validate.status".to_owned(),
            ],
            "a workspace with no remote target cannot dispatch one"
        );

        let with_remote = super::registry(
            &workspace,
            artifacts,
            0,
            Reachable {
                remote_targets: vec![(
                    "build".to_owned(),
                    arsy_kernel::config::RemoteTarget::Container {
                        engine: "docker".to_owned(),
                        container: "builder".to_owned(),
                    },
                )],
                ..Reachable::default()
            },
            "test",
            TurnState::default(),
        )
        .unwrap();
        assert!(with_remote
            .kinds()
            .any(|kind| kind.as_str() == "remote.exec"));
        for kind in &kinds {
            let contract = registry
                .contract(&OperationKind::new(kind.clone()).unwrap())
                .expect("a listed kind has a contract");
            assert!(!contract.actions.is_empty(), "{kind} declares no action");
        }
    }
}
