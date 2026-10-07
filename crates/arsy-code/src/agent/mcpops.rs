//! `mcp.call`: invoking a tool that lives on another process.
//!
//! # Why one operation and many tools
//!
//! An MCP server publishes its own tools with its own schemas, and which ones
//! exist is not known until a connection is made. The compiled-in [`TOOLS`]
//! table cannot describe them, and giving the model a single `mcp_call(server,
//! tool, arguments)` would hide every schema behind an opaque blob it would
//! have to guess at.
//!
//! So the model sees one tool per discovered tool — named `mcp__<server>__<tool>`,
//! carrying the server's own schema — and all of them decode into this single
//! operation. That keeps the audit trail and the policy decision in one place:
//! a call is `mcp.invoke` over `mcp:<server>/<tool>`, so a rule can admit one
//! server, or one tool on one server, and refuse the rest.
//!
//! # Failure is a result, not a crash
//!
//! A server that is slow, gone, or broken produces a bounded error string the
//! model can read and act on. It does not take the turn down, and it does not
//! retry forever: reconnection is the connection's business, and this reports
//! whatever the connection then says.

use crate::mcp::{Connection, McpError};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

/// The most text one MCP tool result may contribute before it is cut.
///
/// Larger than what a turn shows, so the truncation a model sees is decided
/// once, on the rendered text, rather than twice with different limits.
const MAX_RESULT_BYTES: usize = 256 * 1024;

/// Live connections, shared by the executor and by whoever opened them.
pub type Connections = Arc<Mutex<BTreeMap<String, Connection>>>;

/// The longest a call waits for its server to finish connecting.
const CONNECT_WAIT: Duration = Duration::from_secs(60);

/// Servers still connecting in the background.
///
/// A session offers a server's tools before its connection is up, from what
/// the server published last time, so a slow server never holds a turn back.
/// A call to one of those tools waits here for the connection rather than
/// failing because it asked too early.
#[derive(Clone, Default)]
pub struct Pending(Arc<(Mutex<BTreeMap<String, usize>>, Condvar)>);

// Counted per server: a changed definition can start a second attempt while
// the first is still open, and the first finishing must not end the wait for
// the second.
impl Pending {
    pub fn start(&self, server: &str) {
        if let Ok(mut connecting) = self.0 .0.lock() {
            *connecting.entry(server.to_owned()).or_default() += 1;
        }
    }

    /// The connection attempt is over, whichever way it went.
    pub fn finish(&self, server: &str) {
        if let Ok(mut connecting) = self.0 .0.lock() {
            if let Some(count) = connecting.get_mut(server) {
                *count -= 1;
                if *count == 0 {
                    connecting.remove(server);
                }
            }
        }
        self.0 .1.notify_all();
    }

    pub fn contains(&self, server: &str) -> bool {
        self.0
             .0
            .lock()
            .is_ok_and(|connecting| connecting.contains_key(server))
    }

    /// Wait until `server` is no longer connecting, or `limit` has passed.
    pub fn wait(&self, server: &str, limit: Duration) {
        let Ok(connecting) = self.0 .0.lock() else {
            return;
        };
        let _ = self
            .0
             .1
            .wait_timeout_while(connecting, limit, |connecting| {
                connecting.contains_key(server)
            });
    }
}

#[derive(Debug, Serialize)]
struct CallResult {
    server: String,
    tool: String,
    /// Whether the server itself reported the call as failed. Distinct from
    /// the operation failing: a tool that ran and said no is a result.
    is_error: bool,
    /// The textual content blocks, joined — what the model reads.
    text: String,
    /// The server's full reply, for anything that wants more than the text.
    raw: Value,
}

/// `mcp.load`: bring one server's tools into the session.
///
/// A session can carry hundreds of MCP tools, and every request used to send
/// every schema — tens of thousands of tokens on each call, most of it for
/// servers the task never touches. Instead the model is shown each server by
/// name with its tool names, and asks for one when it needs it; from then on
/// that server's full schemas are offered. Loading reads no workspace and
/// reaches no server — the runtime already knows the tools — so the contract
/// asks for no authority, and the call is recorded like any other.
pub struct McpLoadExecutor {
    contract: OperationContract,
}

impl McpLoadExecutor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new(LOAD_OPERATION).expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([("server".to_owned(), JsonType::String)]),
                    optional: BTreeMap::new(),
                    allow_extra: false,
                },
                actions: Vec::new(),
                idempotency: Idempotency::Idempotent,
                reversible: true,
                concurrency: ConcurrencyRule::Parallel,
            },
        })
    }
}

impl OperationExecutor for McpLoadExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    /// Nothing to do here: what a load answers is the server's tool list,
    /// which the runtime holds, and the runtime records the load itself.
    fn execute(
        &self,
        _request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        Ok(OperationOutcome {
            value: None,
            observed_effects: Vec::new(),
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// The operation and the model-visible tool that loads a server's tools.
pub const LOAD_OPERATION: &str = "mcp.load";

pub struct McpExecutor {
    contract: OperationContract,
    connections: Connections,
    pending: Pending,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl McpExecutor {
    pub fn new(
        connections: Connections,
        pending: Pending,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("mcp.call").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([
                        ("server".to_owned(), JsonType::String),
                        ("tool".to_owned(), JsonType::String),
                    ]),
                    optional: BTreeMap::from([("arguments".to_owned(), JsonType::Object)]),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::McpInvoke],
                // Another process's tool. Nothing here can promise what it
                // does, so it is assumed to do something, and to be unable to
                // take it back.
                idempotency: Idempotency::Effectful,
                reversible: false,
                // Per resource rather than global: two servers are two
                // processes and have no reason to wait for each other, but one
                // server speaks a single JSON-RPC channel.
                concurrency: ConcurrencyRule::ExclusivePerResource,
            },
            connections,
            pending,
            artifacts,
            retain_until_ms,
        })
    }

    /// Call one tool, waiting for a server still connecting.
    ///
    /// A connection that broke under the call is dropped, so whoever holds the
    /// session reconnects it at the next turn instead of this call retrying.
    fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<Value, OperationError> {
        self.pending.wait(server, CONNECT_WAIT);
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| OperationError::Execution("the MCP connections are poisoned".into()))?;
        let connection = connections.get_mut(server).ok_or_else(|| {
            OperationError::Execution(format!("the MCP server `{server}` is not connected"))
        })?;
        connection.call_tool(tool, arguments).map_err(|error| {
            if error.is_retryable() {
                connections.remove(server);
            }
            failed(error)
        })
    }
}

impl OperationExecutor for McpExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let string = |key: &str| {
            request
                .input
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let server = string("server");
        let tool = string("tool");
        let arguments = request
            .input
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let reply = self.call(&server, &tool, arguments)?;

        let result = CallResult {
            is_error: reply
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            text: content_text(&reply),
            server: server.clone(),
            tool: tool.clone(),
            raw: reply,
        };
        let value = super::store(
            self.artifacts.as_ref(),
            &result,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::McpInvoke,
                resource: resource(&server, &tool),
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// What a call needs authority over: `mcp:<server>/<tool>`.
///
/// Public because the requirement is built from the request's input before
/// dispatch, and both sides have to agree on the spelling or a rule written
/// for one would not match the other.
pub fn resource(server: &str, tool: &str) -> ResourceRef {
    ResourceRef::new("mcp", resource_path(server, tool))
        .unwrap_or_else(|_| ResourceRef::new("mcp", "*").expect("a static scheme and value"))
}

/// The value half of that reference, for the requirement built before
/// dispatch. One spelling, because a rule written against what the effect
/// records has to match what the requirement asked for.
pub fn resource_path(server: &str, tool: &str) -> String {
    format!("{server}/{tool}")
}

/// The readable part of an MCP result.
///
/// The protocol returns a list of typed content blocks; a model wants the text
/// ones. Anything else is named rather than dumped, so an image block says
/// that an image came back instead of contributing a megabyte of base64 to the
/// turn's context.
fn content_text(reply: &Value) -> String {
    let blocks = reply
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(body) = block.get("text").and_then(Value::as_str) {
                    text.push_str(body);
                    text.push('\n');
                }
            }
            Some(kind) => text.push_str(&format!("[{kind} content, not shown]\n")),
            None => {}
        }
        if text.len() >= MAX_RESULT_BYTES {
            break;
        }
    }
    if text.len() > MAX_RESULT_BYTES {
        let mut cut = MAX_RESULT_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[the rest of this result was omitted]");
    }
    if text.trim().is_empty() {
        // A result with no text at all is still an answer; saying so beats
        // returning an empty string a model would read as a failure.
        "(the tool returned no text)".to_owned()
    } else {
        text.trim_end().to_owned()
    }
}

/// An MCP failure as something the model can act on.
fn failed(error: McpError) -> OperationError {
    OperationError::Execution(format!(
        "the MCP server could not run the tool: {error}. Try another approach, or ask the \
         operator to check the connection with `arsy mcp test`."
    ))
}

/// The model-visible name of one discovered tool.
///
/// Prefixed with the server so two servers offering `search` are two different
/// tools, and doubly underscored so a name cannot collide with a built-in like
/// `fs.read` or be mistaken for one.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

/// Keep a name to what every provider accepts in a tool name.
///
/// A server picks its own names and some providers reject anything outside
/// `[A-Za-z0-9_-]`. Substituting rather than refusing means an awkwardly named
/// tool is still reachable; the original is kept in the binding, so the call
/// still goes out under the name the server published.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The model-facing tools one connection's discovery admits.
pub fn tools_of(connection: &Connection) -> Vec<super::DynamicTool> {
    let server = connection.name().to_owned();
    connection
        .discovery()
        .tools
        .iter()
        .map(|descriptor| super::DynamicTool {
            name: tool_name(&server, &descriptor.name),
            description: descriptor
                .description
                .clone()
                .or_else(|| descriptor.title.clone())
                .unwrap_or_else(|| format!("`{}` on the MCP server `{server}`.", descriptor.name)),
            // A server that published no schema still gets a callable tool:
            // an open object lets the model pass what the description implies
            // rather than leaving the tool unusable.
            input_schema: descriptor.input_schema.clone().unwrap_or_else(
                || serde_json::json!({"type": "object", "additionalProperties": true}),
            ),
            operation: "mcp.call",
            server: server.clone(),
            tool: descriptor.name.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::domain::Principal;

    #[test]
    fn an_older_attempt_finishing_does_not_end_a_newer_one() {
        let pending = Pending::default();
        pending.start("docs");
        pending.start("docs");
        pending.finish("docs");
        assert!(pending.contains("docs"));
        pending.finish("docs");
        assert!(!pending.contains("docs"));
    }

    /// Run a call the way an operator at a keyboard would. `mcp.call` is
    /// irreversible by contract, so policy asks — and the unattended path
    /// refuses rather than deciding for them.
    fn attended(
        runtime: &crate::agent::ToolRuntime,
        tool: &str,
        arguments: &Value,
    ) -> crate::agent::ToolResult {
        match runtime.prepare(tool, arguments) {
            Err(failure) => *failure,
            Ok(request) => match runtime.authorize(&request).approve() {
                Ok(grants) => runtime.dispatch(tool, &request, &grants, std::time::Instant::now()),
                Err(reason) => crate::agent::ToolResult::refused(tool, reason),
            },
        }
    }

    #[test]
    fn a_model_visible_name_is_scoped_to_its_server_and_safe_to_send() {
        assert_eq!(
            tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        // Two servers offering the same tool are two different tools.
        assert_ne!(tool_name("a", "search"), tool_name("b", "search"));
        // Characters a provider would reject are substituted, not refused.
        assert_eq!(
            tool_name("my server", "read:file"),
            "mcp__my_server__read_file"
        );
    }

    #[test]
    fn a_result_is_reduced_to_its_text_and_non_text_blocks_are_named_not_dumped() {
        let reply = serde_json::json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "image", "data": "AAAA"},
                {"type": "text", "text": "second"},
            ],
        });
        assert_eq!(
            content_text(&reply),
            "first\n[image content, not shown]\nsecond"
        );
        assert_eq!(
            content_text(&serde_json::json!({"content": []})),
            "(the tool returned no text)"
        );
    }

    #[test]
    fn a_call_is_authorized_against_the_server_and_the_tool_it_names() {
        let reference = resource("github", "create_issue");
        assert_eq!(reference.scheme(), "mcp");
        assert_eq!(reference.value(), "github/create_issue");
    }

    /// A server that answers from a script, so the whole path — negotiate,
    /// discover, offer, call, render — can be exercised without a subprocess.
    struct FakeServer;

    impl crate::mcp::Channel for FakeServer {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
            Ok(match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0"},
                }),
                "tools/list" => serde_json::json!({
                    "tools": [{
                        "name": "add",
                        "description": "Add two numbers.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                            "required": ["a", "b"],
                        },
                    }],
                }),
                "tools/call" if params["name"] == "add" => {
                    let sum = params["arguments"]["a"].as_i64().unwrap_or(0)
                        + params["arguments"]["b"].as_i64().unwrap_or(0);
                    serde_json::json!({"content": [{"type": "text", "text": sum.to_string()}]})
                }
                "tools/call" => serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": "no such tool"}],
                }),
                other => return Err(McpError::Protocol(format!("unexpected {other}"))),
            })
        }

        fn notify(&mut self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }

        fn close(&mut self) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct FakeChannels;

    impl crate::mcp::ChannelFactory for FakeChannels {
        fn connect(
            &self,
            _definition: &arsy_kernel::config::McpServer,
        ) -> Result<Box<dyn crate::mcp::Channel>, McpError> {
            Ok(Box::new(FakeServer))
        }
    }

    fn definition() -> arsy_kernel::config::McpServer {
        arsy_kernel::config::McpServer {
            name: "calc".to_owned(),
            transport: arsy_kernel::config::McpTransport::Stdio {
                command: "unused".to_owned(),
                args: Vec::new(),
                env: Default::default(),
            },
            enabled: true,
            trust: arsy_kernel::capability::PolicySource::User,
            timeout_ms: 5_000,
            max_body_bytes: 1024 * 1024,
        }
    }

    /// A tool offered before its server finished connecting still works: the
    /// call waits for the connection, and a server nobody is connecting fails
    /// straight away rather than waiting.
    #[test]
    fn a_call_waits_for_a_server_that_is_still_connecting() {
        let directory = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(
            arsy_kernel::artifact::FileArtifactStore::open(directory.path().join("art"), 0)
                .unwrap(),
        );
        let connections: Connections = Arc::default();
        let pending = Pending::default();
        pending.start("calc");
        let executor = McpExecutor::new(Arc::clone(&connections), pending.clone(), artifacts, 0);

        let (opened, finished) = (Arc::clone(&connections), pending.clone());
        let opener = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let connection =
                crate::mcp::Connection::open(&definition(), None, &FakeChannels).unwrap();
            opened.lock().unwrap().insert("calc".to_owned(), connection);
            finished.finish("calc");
        });
        let reply = executor
            .call("calc", "add", serde_json::json!({"a": 2, "b": 3}))
            .unwrap();
        opener.join().unwrap();
        assert_eq!(content_text(&reply), "5");
        assert!(!pending.contains("calc"));

        let started = std::time::Instant::now();
        assert!(executor.call("gone", "add", serde_json::json!({})).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The whole path a turn takes: connect, discover, offer the tool under a
    /// model-visible name, call it, and read the server's own answer back.
    #[test]
    fn a_discovered_tool_is_offered_and_called_through_the_ordinary_runtime() {
        use arsy_kernel::{
            capability::PolicySource,
            policy::{
                ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance,
                WorkspaceCleanliness,
            },
        };

        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::resource::Workspace::open(directory.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(
            arsy_kernel::artifact::FileArtifactStore::open(directory.path().join("art"), 0)
                .unwrap(),
        );

        let connection = crate::mcp::Connection::open(&definition(), None, &FakeChannels).unwrap();
        let discovered = tools_of(&connection);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name, "mcp__calc__add");
        assert_eq!(
            discovered[0].input_schema["required"],
            serde_json::json!(["a", "b"]),
            "the server's own schema is what the model is shown"
        );

        let connections: Connections = Arc::new(Mutex::new(BTreeMap::from([(
            "calc".to_owned(),
            connection,
        )])));
        let rules = RuleSet::compile(CapabilityAction::ALL.iter().map(|action| {
            PolicyRule {
                source: PolicySource::User,
                effect: RuleEffect::Allow,
                actor: ActorMatch::Any,
                action: *action,
                pattern: arsy_kernel::capability::ResourcePattern::new(
                    action.default_scheme(),
                    "**",
                )
                .unwrap(),
                expires_at_ms: None,
                delegation_depth: 0,
                minimum_assurance: SandboxAssurance::None,
            }
        }));
        let runtime = crate::agent::runtime(
            &workspace,
            rules,
            artifacts,
            0,
            Principal::System,
            RiskContext {
                reversible: true,
                workspace: WorkspaceCleanliness::Clean,
                sandbox: SandboxAssurance::None,
            },
            crate::operations::Reachable::default(),
            "test",
            crate::operations::TurnState {
                mcp: Some(connections),
                ..crate::operations::TurnState::default()
            },
            &[],
        )
        .unwrap()
        .with_dynamic_tools(discovered);

        // Offered on demand: until the model loads the server, it sees the
        // server and its tool names in `mcp.load`, not every full schema.
        let offered = |name: &str| {
            runtime
                .schemas()
                .into_iter()
                .find(|schema| schema.name == name)
        };
        assert!(
            offered("mcp__calc__add").is_none(),
            "not sent before it is loaded"
        );
        let load = offered(LOAD_OPERATION).expect("mcp.load is offered while a server waits");
        assert!(
            load.description.contains("- calc: add"),
            "{}",
            load.description
        );

        let unknown = attended(
            &runtime,
            LOAD_OPERATION,
            &serde_json::json!({"server": "nope"}),
        );
        assert!(!unknown.success);
        assert!(unknown.output.contains("calc"), "{}", unknown.output);

        let loaded = attended(
            &runtime,
            LOAD_OPERATION,
            &serde_json::json!({"server": "calc"}),
        );
        assert!(loaded.success, "{}", loaded.output);
        assert!(
            loaded.output.contains("mcp__calc__add"),
            "{}",
            loaded.output
        );
        assert!(
            offered("mcp__calc__add").is_some(),
            "a loaded server's tools are offered beside the built-in ones"
        );
        assert!(offered(LOAD_OPERATION).is_none(), "nothing is left to load");

        let result = attended(
            &runtime,
            "mcp__calc__add",
            &serde_json::json!({"a": 2, "b": 3}),
        );
        assert!(result.success, "{}", result.output);
        assert_eq!(result.output, "5");
        assert!(
            result.artifact.is_some(),
            "an MCP call leaves the same evidence every other call does"
        );

        // A tool the server refuses is a failed result the model can read, not
        // a JSON dump it has to notice `isError` inside.
        let refused = attended(
            &runtime,
            "mcp__calc__add",
            &serde_json::json!({"a": "not", "b": "numbers"}),
        );
        assert!(refused.success, "arguments are the server's business");

        // A name no connection offers is refused with the list that exists.
        let unknown = attended(&runtime, "mcp__calc__subtract", &serde_json::json!({}));
        assert!(!unknown.success);
        assert!(
            unknown.output.contains("mcp__calc__add"),
            "{}",
            unknown.output
        );
    }

    /// Without a connection there is no `mcp.call` executor, so a leftover
    /// tool must not be advertised as something the model could call.
    #[test]
    fn a_discovered_tool_is_not_offered_when_nothing_can_dispatch_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::resource::Workspace::open(directory.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(
            arsy_kernel::artifact::FileArtifactStore::open(directory.path().join("art"), 0)
                .unwrap(),
        );
        let runtime = crate::agent::runtime(
            &workspace,
            arsy_kernel::policy::RuleSet::default(),
            artifacts,
            0,
            Principal::System,
            arsy_kernel::policy::RiskContext {
                reversible: true,
                workspace: arsy_kernel::policy::WorkspaceCleanliness::Clean,
                sandbox: arsy_kernel::policy::SandboxAssurance::None,
            },
            crate::operations::Reachable::default(),
            "test",
            crate::operations::TurnState::default(),
            &[],
        )
        .unwrap()
        .with_dynamic_tools(vec![super::super::DynamicTool {
            name: "mcp__gone__thing".to_owned(),
            description: "stale".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            operation: "mcp.call",
            server: "gone".to_owned(),
            tool: "thing".to_owned(),
        }]);

        assert!(!runtime
            .schemas()
            .iter()
            .any(|schema| schema.name == "mcp__gone__thing"));
    }
}
