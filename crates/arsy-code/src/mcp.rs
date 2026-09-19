//! MCP client: stdio and Streamable HTTP, under a capability ceiling.
//!
//! See `docs/24-mcp-acp.md`. Two properties decide the shape of this module:
//!
//! * **Reconnecting never widens authority.** The set of tools, resources, and
//!   prompts adopted on the first successful connection becomes the ceiling.
//!   Anything that appears later — after a reconnect or a refresh — and is not
//!   in that ceiling is rejected and reported, never adopted. Otherwise
//!   dropping a connection would be a way to acquire capability.
//! * **Nothing from a server is trusted input.** Bodies are capped, requests
//!   are deadlined, and a server-initiated request is not honoured: sampling
//!   and elicitation need a policy decision this layer does not own, so they
//!   are refused rather than answered.
//!
//! Raw JSON-RPC stops here. Callers see `Descriptor`, `Discovery`, and
//! `McpError`.

use arsy_kernel::{
    config::{LaunchEnv, McpServer, McpTransport},
    provider::wire::{header, WireRequest, WireTransport},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fmt,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// Protocol revision this client offers. The server answers with the revision
/// it will actually speak, which is what the session records.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Longest single line accepted from a server, before the body cap applies.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// What a client identifies itself as during negotiation.
const CLIENT_NAME: &str = "arsy";

// -- Errors ----------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpError {
    /// The connection is configured off; connecting needs an explicit enable.
    Disabled(String),
    /// The server rejected the credential. Never retried automatically.
    Auth(String),
    /// Spawn, socket, or stream failure. Retryable.
    Transport(String),
    /// The server answered, but not with something this protocol allows.
    Protocol(String),
    /// The server took longer than the connection's deadline.
    Timeout(Duration),
    /// A response exceeded the connection's body cap.
    BodyTooLarge { bytes: u64, max: u64 },
    /// The server reported a JSON-RPC error.
    Server { code: i64, message: String },
    /// The name is outside the ceiling this connection was admitted with.
    OutsideCeiling(String),
}

impl McpError {
    /// Whether a supervisor may retry this on its own.
    ///
    /// A rejected credential and a disabled connection are excluded: retrying
    /// either turns an operator's decision into a loop.
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Timeout(_) | Self::Protocol(_)
        )
    }

    /// The diagnostic class this failure belongs to, per `docs/33-diagnostics.md`.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Disabled(_) | Self::OutsideCeiling(_) => "ARSY-POL-1100",
            Self::Auth(_) => "ARSY-CRD-1100",
            Self::Transport(_) | Self::Timeout(_) | Self::BodyTooLarge { .. } => "ARSY-PRT-1100",
            Self::Protocol(_) | Self::Server { .. } => "ARSY-PRT-1101",
        }
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled(name) => write!(formatter, "connection `{name}` is disabled"),
            Self::Auth(detail) => write!(formatter, "the server rejected the credential: {detail}"),
            Self::Transport(detail) => write!(formatter, "transport failed: {detail}"),
            Self::Protocol(detail) => write!(formatter, "protocol violation: {detail}"),
            Self::Timeout(limit) => write!(formatter, "no answer within {limit:?}"),
            Self::BodyTooLarge { bytes, max } => {
                write!(formatter, "response of {bytes} bytes exceeds the {max} cap")
            }
            Self::Server { code, message } => {
                write!(formatter, "server error {code}: {message}")
            }
            Self::OutsideCeiling(name) => write!(
                formatter,
                "`{name}` is outside the capability ceiling this connection holds"
            ),
        }
    }
}

impl std::error::Error for McpError {}

// -- Discovery -------------------------------------------------------------

/// One tool, resource, or prompt a server offers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Descriptor {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The tool's JSON Schema, as the server published it.
    ///
    /// Carried through discovery rather than fetched later, because it is what
    /// makes a discovered tool callable: a model handed a name with no
    /// parameter shape can only guess at the arguments. `None` for resources
    /// and prompts, which are not called with arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

/// Something the server offered that this connection refused, and why.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Rejected {
    pub kind: &'static str,
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Discovery {
    pub tools: Vec<Descriptor>,
    pub resources: Vec<Descriptor>,
    pub prompts: Vec<Descriptor>,
    /// Entries outside the ceiling. Reported, never adopted.
    pub rejected: Vec<Rejected>,
}

impl Discovery {
    /// The names this discovery admits, which is what a first connection
    /// records as its ceiling.
    pub fn ceiling(&self) -> Ceiling {
        Ceiling {
            tools: self.tools.iter().map(|it| it.name.clone()).collect(),
            resources: self.resources.iter().map(|it| it.name.clone()).collect(),
            prompts: self.prompts.iter().map(|it| it.name.clone()).collect(),
        }
    }
}

/// Everything one connection is allowed to expose.
///
/// A ceiling is a set of exact names rather than a pattern: it is derived from
/// what was actually adopted, so there is nothing to interpret and nothing a
/// later, differently-worded server response can widen.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Ceiling {
    pub tools: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub prompts: BTreeSet<String>,
}

impl Ceiling {
    fn admits(&self, kind: &str, name: &str) -> bool {
        match kind {
            "tool" => self.tools.contains(name),
            "resource" => self.resources.contains(name),
            _ => self.prompts.contains(name),
        }
    }
}

/// What a refresh or a reconnect changed, for the event both append.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiscoveryChange {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub rejected: Vec<Rejected>,
}

impl DiscoveryChange {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.rejected.is_empty()
    }
}

/// Negotiated identity of the far side.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// The revision the server chose, which may differ from what was offered.
    pub protocol_version: String,
}

// -- Transports ------------------------------------------------------------

/// One JSON-RPC channel to a server.
pub trait Channel: Send {
    /// Send a request and return its result, or the server's error.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Send a notification, which has no reply.
    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError>;
    fn close(&mut self) -> Result<(), McpError>;
}

/// Where a server's own log lines go.
///
/// A server logs to its stderr, and those lines are the operator's — but the
/// process stderr is not always somewhere safe to put them. An interactive
/// session paints its frame in place, so a write from one of these threads
/// lands in the middle of whatever is on screen. The caller therefore says
/// where they go, and only the caller knows whether anything is drawing.
///
/// The line arrives already scrubbed of the launch values, and it is untrusted
/// content: a sink displays it, never acts on it.
pub type McpLogSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The default: this process's stderr, which is right for a scripted run.
pub fn stderr_log_sink() -> McpLogSink {
    Arc::new(|server: &str, line: &str| {
        let _ = writeln!(io::stderr(), "{server}: {line}");
    })
}

/// A child process speaking newline-delimited JSON-RPC on its stdio.
pub struct StdioChannel {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<Result<String, String>>,
    timeout: Duration,
    max_body_bytes: u64,
    next_id: u64,
}

impl StdioChannel {
    /// `env` is added to the environment the child inherits, so a server
    /// declared with a connection string or token receives it and nothing
    /// else does.
    pub fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &LaunchEnv,
        timeout: Duration,
        max_body_bytes: u64,
        log: &McpLogSink,
    ) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server's own logs belong on the operator's terminal, not
            // mixed into the protocol stream -- but forwarded rather than
            // inherited. An inherited handle outlives the child: a server that
            // leaves a helper running hands that helper the caller's stderr,
            // and whoever is reading it then waits on the helper rather than on
            // the probe's deadline.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| McpError::Transport(format!("cannot start `{command}`: {error}")))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let secrets = scrubbed_values(env);
        let log = Arc::clone(log);
        let name = name.to_owned();
        thread::spawn(move || {
            // A server that logs its own connection string or token must not
            // put it on the operator's terminal, so each line is scrubbed of
            // the values this definition handed it before the sink sees it.
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log(&name, &scrub(&line, &secrets));
            }
        });
        let (sender, lines) = mpsc::channel();
        // A reader thread is what makes a deadline possible: a blocking read on
        // a child that never answers cannot be interrupted otherwise.
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut buffer = Vec::new();
                let read = (&mut reader)
                    .take(MAX_LINE_BYTES as u64 + 1)
                    .read_until(b'\n', &mut buffer);
                let message = match read {
                    Ok(0) => return,
                    Ok(read) if read > MAX_LINE_BYTES => {
                        let _ = sender.send(Err(format!("line exceeded {MAX_LINE_BYTES} bytes")));
                        return;
                    }
                    Ok(_) => String::from_utf8(buffer)
                        .map_err(|_| "server output is not UTF-8".to_owned()),
                    Err(error) => Err(error.to_string()),
                };
                let failed = message.is_err();
                if sender.send(message).is_err() || failed {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            timeout,
            max_body_bytes,
            next_id: 1,
        })
    }

    fn write(&mut self, message: &Value) -> Result<(), McpError> {
        let encoded = serde_json::to_string(message)
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        writeln!(self.stdin, "{encoded}")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| McpError::Transport(error.to_string()))
    }

    /// Read until the reply to `id` arrives, the deadline passes, or the body
    /// cap is reached.
    ///
    /// Anything else on the stream is skipped: a notification is not an answer,
    /// and a server-initiated request is refused by not answering it, because
    /// honouring sampling or elicitation needs a decision this layer cannot make.
    fn read_reply(&mut self, id: u64) -> Result<Value, McpError> {
        let deadline = Instant::now() + self.timeout;
        let mut consumed = 0_u64;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(McpError::Timeout(self.timeout))?;
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => return Err(McpError::Transport(error)),
                Err(RecvTimeoutError::Timeout) => return Err(McpError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Transport("the server closed its output".into()))
                }
            };
            consumed = consumed.saturating_add(line.len() as u64);
            if consumed > self.max_body_bytes {
                return Err(McpError::BodyTooLarge {
                    bytes: consumed,
                    max: self.max_body_bytes,
                });
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(line.trim())
                .map_err(|error| McpError::Protocol(format!("{error}: {line}")))?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return unwrap_result(&message);
            }
        }
    }
}

impl Channel for StdioChannel {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.read_reply(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn close(&mut self) -> Result<(), McpError> {
        // Closing stdin is how an MCP stdio server is asked to exit; the kill
        // is the fallback for one that ignores it.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

/// A failed connection must not leave the server running.
///
/// Without this, a negotiation that times out drops the channel while the child
/// lives on holding this process's inherited stderr, so the *caller* blocks
/// until the server decides to exit. Cleanup belongs to the value that owns the
/// process, not to the one path that happens to close it politely.
impl Drop for StdioChannel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The launch values worth hiding. Very short ones are left alone: replacing
/// every `1` or `on` in a log would hide the log, not a secret.
fn scrubbed_values(env: &LaunchEnv) -> Vec<String> {
    let mut values: Vec<String> = env
        .iter()
        .map(|(_, value)| value.to_owned())
        .filter(|value| value.len() >= 6)
        .collect();
    // Longest first, so a value containing another is replaced whole.
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values
}

fn scrub(line: &str, secrets: &[String]) -> String {
    secrets.iter().fold(line.to_owned(), |line, secret| {
        line.replace(secret.as_str(), "[redacted]")
    })
}

/// Streamable HTTP: one POST per request, answering with JSON or SSE.
pub struct HttpChannel {
    url: String,
    transport: Box<dyn WireTransport>,
    /// `Mcp-Session-Id` from the initialize response, replayed on every later
    /// request so the server can correlate them.
    session: Option<String>,
    /// Headers the definition carries, such as an authorization token. A name
    /// the protocol sets itself is dropped, so a definition cannot rewrite the
    /// framing of its own messages.
    headers: Vec<(String, String)>,
    max_body_bytes: u64,
    next_id: u64,
}

impl HttpChannel {
    pub fn new(
        url: impl Into<String>,
        headers: &LaunchEnv,
        transport: Box<dyn WireTransport>,
        max_body_bytes: u64,
    ) -> Self {
        Self {
            url: url.into(),
            transport,
            session: None,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
                .filter(|(name, _)| {
                    !matches!(
                        name.as_str(),
                        "content-type" | "accept" | "mcp-protocol-version" | "mcp-session-id"
                    )
                })
                .collect(),
            max_body_bytes,
            next_id: 1,
        }
    }

    fn post(&mut self, message: &Value, expect_reply: Option<u64>) -> Result<Value, McpError> {
        let mut headers = self.headers.clone();
        headers.extend([
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            (
                "mcp-protocol-version".to_owned(),
                PROTOCOL_VERSION.to_owned(),
            ),
        ]);
        if let Some(session) = &self.session {
            headers.push(("mcp-session-id".to_owned(), session.clone()));
        }
        let response = self
            .transport
            .send(WireRequest {
                url: self.url.clone(),
                headers,
                body: serde_json::to_string(message)
                    .map_err(|error| McpError::Protocol(error.to_string()))?,
            })
            .map_err(|error| McpError::Transport(error.to_string()))?;

        if matches!(response.status, 401 | 403) {
            return Err(McpError::Auth(format!("HTTP {}", response.status)));
        }
        if let Some(session) = header(&response.headers, "mcp-session-id") {
            self.session = Some(session.to_owned());
        }
        let sse = header(&response.headers, "content-type")
            .is_some_and(|value| value.starts_with("text/event-stream"));
        let status = response.status;

        let mut consumed = 0_u64;
        let mut plain = String::new();
        for line in response.lines {
            let line = line.map_err(McpError::Transport)?;
            consumed = consumed.saturating_add(line.len() as u64);
            if consumed > self.max_body_bytes {
                return Err(McpError::BodyTooLarge {
                    bytes: consumed,
                    max: self.max_body_bytes,
                });
            }
            let payload = if sse {
                match line.strip_prefix("data:") {
                    Some(payload) => payload.trim(),
                    None => continue,
                }
            } else {
                plain.push_str(&line);
                continue;
            };
            let message: Value = serde_json::from_str(payload)
                .map_err(|error| McpError::Protocol(format!("{error}: {payload}")))?;
            match expect_reply {
                None => return Ok(Value::Null),
                Some(id) if message.get("id").and_then(Value::as_u64) == Some(id) => {
                    return unwrap_result(&message)
                }
                Some(_) => continue,
            }
        }

        let Some(id) = expect_reply else {
            // A notification is answered with 202 and no body; anything 2xx is
            // an acknowledgement.
            return if (200..300).contains(&status) {
                Ok(Value::Null)
            } else {
                Err(McpError::Transport(format!("HTTP {status}")))
            };
        };
        if plain.trim().is_empty() {
            return Err(McpError::Transport(format!(
                "HTTP {status} carried no reply to request {id}"
            )));
        }
        let message: Value = serde_json::from_str(plain.trim())
            .map_err(|error| McpError::Protocol(format!("{error}: {plain}")))?;
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(McpError::Protocol(format!(
                "reply is for another request than {id}"
            )));
        }
        unwrap_result(&message)
    }
}

impl Channel for HttpChannel {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.post(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            Some(id),
        )
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        self.post(
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
            None,
        )
        .map(|_| ())
    }

    fn close(&mut self) -> Result<(), McpError> {
        self.session = None;
        Ok(())
    }
}

/// A JSON-RPC envelope reduced to its result, or to the error it carried.
fn unwrap_result(message: &Value) -> Result<Value, McpError> {
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message")
            .to_owned();
        // -32001 and -32002 are what servers use for an unauthorized session;
        // both are an operator's problem, not something to retry into.
        return Err(if matches!(code, -32001 | -32002) {
            McpError::Auth(text)
        } else {
            McpError::Server {
                code,
                message: text,
            }
        });
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

// -- Connection ------------------------------------------------------------

/// A negotiated connection to one server.
pub struct Connection {
    name: String,
    channel: Box<dyn Channel>,
    server: ServerInfo,
    capabilities: Value,
    ceiling: Ceiling,
    discovery: Discovery,
}

impl Connection {
    /// Open `definition`, negotiate, and discover what it offers.
    ///
    /// `ceiling` is the set this connection already held. `None` is a first
    /// connection: what it discovers becomes its ceiling. Anything else is a
    /// reconnect, and discovery is filtered against what was already granted.
    pub fn open(
        definition: &McpServer,
        ceiling: Option<Ceiling>,
        transport: &dyn ChannelFactory,
    ) -> Result<Self, McpError> {
        if !definition.enabled {
            return Err(McpError::Disabled(definition.name.clone()));
        }
        let mut channel = transport.connect(definition)?;
        let negotiated = channel.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": crate::VERSION},
            }),
        )?;
        let server = ServerInfo {
            name: text(&negotiated["serverInfo"]["name"]).unwrap_or_else(|| "unknown".to_owned()),
            version: text(&negotiated["serverInfo"]["version"])
                .unwrap_or_else(|| "unknown".to_owned()),
            protocol_version: text(&negotiated["protocolVersion"]).ok_or_else(|| {
                McpError::Protocol("initialize did not name a protocol version".into())
            })?,
        };
        channel.notify("notifications/initialized", json!({}))?;

        let capabilities = negotiated["capabilities"].clone();
        let discovery = discover(channel.as_mut(), &capabilities, ceiling.as_ref())?;
        Ok(Self {
            name: definition.name.clone(),
            channel,
            server,
            capabilities,
            // A first connection is bounded by what it actually adopted, so a
            // later reconnect has something to be checked against.
            ceiling: ceiling.unwrap_or_else(|| discovery.ceiling()),
            discovery,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn server(&self) -> &ServerInfo {
        &self.server
    }

    pub const fn discovery(&self) -> &Discovery {
        &self.discovery
    }

    pub const fn ceiling(&self) -> &Ceiling {
        &self.ceiling
    }

    /// Re-run discovery without tearing the connection down.
    ///
    /// The session, its correlations, and its in-flight work are untouched;
    /// only the discovery cache is invalidated. Refreshed entries face the same
    /// ceiling check as newly discovered ones.
    pub fn refresh(&mut self) -> Result<DiscoveryChange, McpError> {
        let before = self.discovery.ceiling();
        let discovery = discover(
            self.channel.as_mut(),
            &self.capabilities,
            Some(&self.ceiling),
        )?;
        let after = discovery.ceiling();
        let change = DiscoveryChange {
            added: difference(&after, &before),
            removed: difference(&before, &after),
            rejected: discovery.rejected.clone(),
        };
        self.discovery = discovery;
        Ok(change)
    }

    /// Invoke a tool this connection is allowed to expose.
    ///
    /// The ceiling is checked here as well as at discovery: a caller holding a
    /// name from an earlier connection must not be able to reach past what this
    /// one was admitted with.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        if !self.ceiling.admits("tool", name) {
            return Err(McpError::OutsideCeiling(name.to_owned()));
        }
        self.channel
            .request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    pub fn close(mut self) -> Result<(), McpError> {
        self.channel.close()
    }
}

/// How a definition becomes a channel. Injected so the protocol is testable
/// without a subprocess or a network.
pub trait ChannelFactory {
    fn connect(&self, definition: &McpServer) -> Result<Box<dyn Channel>, McpError>;
}

/// The real factory: a child process for stdio, an HTTP POST for the rest.
pub struct RealChannels<F> {
    /// Builds the HTTP transport. A closure rather than a value because one
    /// factory serves many connections and each gets its own channel.
    pub http: F,
    /// Where every stdio server's log lines go. See [`McpLogSink`].
    pub log: McpLogSink,
}

impl<F> ChannelFactory for RealChannels<F>
where
    F: Fn() -> Box<dyn WireTransport>,
{
    fn connect(&self, definition: &McpServer) -> Result<Box<dyn Channel>, McpError> {
        match &definition.transport {
            McpTransport::Stdio { command, args, env } => Ok(Box::new(StdioChannel::spawn(
                &definition.name,
                command,
                args,
                env,
                Duration::from_millis(definition.timeout_ms),
                definition.max_body_bytes,
                &self.log,
            )?)),
            McpTransport::Http { url, headers } => Ok(Box::new(HttpChannel::new(
                url,
                headers,
                (self.http)(),
                definition.max_body_bytes,
            ))),
        }
    }
}

/// Open a connection, retrying a retryable failure with bounded backoff.
///
/// The delay list *is* the attempt limit: when it is exhausted the connection
/// is failed and stays failed. A rejected credential or a disabled connection
/// is never retried, so an operator's decision cannot become a loop.
pub fn open_with_backoff(
    definition: &McpServer,
    ceiling: Option<Ceiling>,
    transport: &dyn ChannelFactory,
    restart: &crate::lsp::RestartPolicy,
    sleep: &mut dyn FnMut(Duration),
) -> Result<Connection, McpError> {
    let mut attempt = 0;
    loop {
        match Connection::open(definition, ceiling.clone(), transport) {
            Ok(connection) => return Ok(connection),
            Err(error) if error.is_retryable() && attempt < restart.delays.len() => {
                sleep(restart.delays[attempt]);
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Ask for whatever the server said it has, and keep only what the ceiling
/// admits.
fn discover(
    channel: &mut dyn Channel,
    capabilities: &Value,
    ceiling: Option<&Ceiling>,
) -> Result<Discovery, McpError> {
    let mut discovery = Discovery::default();
    for (capability, method, key, kind) in [
        ("tools", "tools/list", "tools", "tool"),
        ("resources", "resources/list", "resources", "resource"),
        ("prompts", "prompts/list", "prompts", "prompt"),
    ] {
        if capabilities.get(capability).is_none() {
            continue;
        }
        let listed = channel.request(method, json!({}))?;
        let entries = listed
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol(format!("{method} did not return `{key}`")))?;
        for entry in entries {
            // A resource is named by its URI; a tool and a prompt by `name`.
            let name = text(&entry["name"])
                .or_else(|| text(&entry["uri"]))
                .ok_or_else(|| McpError::Protocol(format!("a {kind} has no name")))?;
            if ceiling.is_some_and(|ceiling| !ceiling.admits(kind, &name)) {
                discovery.rejected.push(Rejected {
                    kind,
                    name,
                    reason: "outside the capability ceiling this connection holds".to_owned(),
                });
                continue;
            }
            let descriptor = Descriptor {
                name,
                title: text(&entry["title"]),
                description: text(&entry["description"]),
                input_schema: (kind == "tool")
                    .then(|| entry.get("inputSchema").cloned())
                    .flatten()
                    .filter(Value::is_object),
            };
            match kind {
                "tool" => discovery.tools.push(descriptor),
                "resource" => discovery.resources.push(descriptor),
                _ => discovery.prompts.push(descriptor),
            }
        }
    }
    Ok(discovery)
}

/// Names in `left` that `right` does not have, across all three kinds.
fn difference(left: &Ceiling, right: &Ceiling) -> Vec<String> {
    let mut names: Vec<String> = left
        .tools
        .difference(&right.tools)
        .chain(left.resources.difference(&right.resources))
        .chain(left.prompts.difference(&right.prompts))
        .cloned()
        .collect();
    names.sort();
    names
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A server that answers from a script, so the protocol is exercised
    /// without a process or a socket.
    #[derive(Default)]
    struct Scripted {
        tools: Arc<Mutex<Vec<&'static str>>>,
        calls: Arc<Mutex<Vec<String>>>,
        fail_first: Arc<Mutex<u32>>,
    }

    impl Channel for Scripted {
        fn request(&mut self, method: &str, _params: Value) -> Result<Value, McpError> {
            self.calls.lock().unwrap().push(method.to_owned());
            Ok(match method {
                "initialize" => json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "scripted", "version": "1"},
                }),
                "tools/list" => json!({
                    "tools": self
                        .tools
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| json!({"name": name}))
                        .collect::<Vec<_>>()
                }),
                "tools/call" => json!({"content": []}),
                other => return Err(McpError::Protocol(format!("unscripted {other}"))),
            })
        }

        fn notify(&mut self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }

        fn close(&mut self) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct Factory(Scripted);

    impl ChannelFactory for Factory {
        fn connect(&self, _definition: &McpServer) -> Result<Box<dyn Channel>, McpError> {
            let mut remaining = self.0.fail_first.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err(McpError::Transport("connection refused".into()));
            }
            Ok(Box::new(Scripted {
                tools: Arc::clone(&self.0.tools),
                calls: Arc::clone(&self.0.calls),
                fail_first: Arc::clone(&self.0.fail_first),
            }))
        }
    }

    fn definition(enabled: bool) -> McpServer {
        McpServer {
            name: "docs".to_owned(),
            transport: McpTransport::Stdio {
                command: "unused".to_owned(),
                args: Vec::new(),
                env: Default::default(),
            },
            enabled,
            trust: arsy_kernel::capability::PolicySource::User,
            timeout_ms: 1_000,
            max_body_bytes: 1024,
        }
    }

    #[test]
    fn a_first_connection_adopts_what_it_discovers_as_its_ceiling() {
        let factory = Factory(Scripted {
            tools: Arc::new(Mutex::new(vec!["search", "fetch"])),
            ..Scripted::default()
        });
        let connection = Connection::open(&definition(true), None, &factory).unwrap();
        assert_eq!(connection.server().name, "scripted");
        assert_eq!(connection.server().protocol_version, "2026-07-28");
        assert_eq!(connection.discovery().tools.len(), 2);
        assert_eq!(
            connection.ceiling().tools,
            ["fetch".to_owned(), "search".to_owned()].into()
        );
        // The server declared no resources or prompts, so neither was asked
        // for: a listing method a server does not implement is not called.
        let asked = factory.0.calls.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec!["initialize".to_owned(), "tools/list".to_owned()]
        );
    }

    #[test]
    fn reconnecting_cannot_acquire_a_tool_the_ceiling_does_not_hold() {
        let factory = Factory(Scripted {
            tools: Arc::new(Mutex::new(vec!["search"])),
            ..Scripted::default()
        });
        let first = Connection::open(&definition(true), None, &factory).unwrap();
        let ceiling = first.ceiling().clone();

        // The server grows a tool while the connection is down.
        *factory.0.tools.lock().unwrap() = vec!["search", "exfiltrate"];
        let mut reconnected = Connection::open(&definition(true), Some(ceiling), &factory).unwrap();
        let names: Vec<_> = reconnected
            .discovery()
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(names, vec!["search".to_owned()]);
        assert_eq!(reconnected.discovery().rejected.len(), 1);
        assert_eq!(reconnected.discovery().rejected[0].name, "exfiltrate");
        assert_eq!(
            reconnected.call_tool("exfiltrate", json!({})),
            Err(McpError::OutsideCeiling("exfiltrate".to_owned())),
            "a rejected tool must not be reachable by name either"
        );
        assert!(reconnected.call_tool("search", json!({})).is_ok());
    }

    #[test]
    fn refresh_reports_what_changed_without_widening_the_ceiling() {
        let factory = Factory(Scripted {
            tools: Arc::new(Mutex::new(vec!["search", "fetch"])),
            ..Scripted::default()
        });
        let mut connection = Connection::open(&definition(true), None, &factory).unwrap();
        *factory.0.tools.lock().unwrap() = vec!["search", "summarize"];

        let change = connection.refresh().unwrap();
        assert!(change.added.is_empty(), "a refresh cannot add authority");
        assert_eq!(change.removed, vec!["fetch".to_owned()]);
        assert_eq!(change.rejected.len(), 1);
        assert_eq!(change.rejected[0].name, "summarize");
        assert_eq!(
            connection.ceiling().tools.len(),
            2,
            "the ceiling is unchanged"
        );
    }

    #[test]
    fn backoff_is_bounded_and_never_retries_a_refusal() {
        let factory = Factory(Scripted {
            tools: Arc::new(Mutex::new(vec!["search"])),
            fail_first: Arc::new(Mutex::new(2)),
            ..Scripted::default()
        });
        let restart = crate::lsp::RestartPolicy {
            delays: vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
        };
        let mut slept = Vec::new();
        let connection =
            open_with_backoff(&definition(true), None, &factory, &restart, &mut |delay| {
                slept.push(delay)
            })
            .unwrap();
        assert_eq!(slept.len(), 2, "one sleep per failed attempt");
        drop(connection);

        // More failures than the policy allows: the connection stays failed.
        *factory.0.fail_first.lock().unwrap() = 9;
        let mut slept = Vec::new();
        let error = open_with_backoff(&definition(true), None, &factory, &restart, &mut |delay| {
            slept.push(delay)
        })
        .err()
        .expect("an exhausted policy leaves the connection failed");
        assert!(error.is_retryable());
        assert_eq!(slept.len(), restart.delays.len());

        // A disabled connection is refused outright, with no attempt at all.
        let mut slept = Vec::new();
        assert_eq!(
            open_with_backoff(&definition(false), None, &factory, &restart, &mut |delay| {
                slept.push(delay)
            })
            .err()
            .expect("a disabled connection is refused"),
            McpError::Disabled("docs".to_owned())
        );
        assert!(slept.is_empty());
    }

    struct Recording(Arc<Mutex<Vec<(String, String)>>>);

    impl WireTransport for Recording {
        fn send(
            &self,
            request: WireRequest,
        ) -> Result<arsy_kernel::provider::wire::WireResponse, arsy_kernel::provider::ProviderError>
        {
            *self.0.lock().unwrap() = request.headers;
            Ok(arsy_kernel::provider::wire::WireResponse {
                status: 401,
                headers: Vec::new(),
                lines: Box::new(std::iter::empty()),
            })
        }
    }

    #[test]
    fn an_http_definition_sends_its_headers_but_not_over_the_protocol() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let headers = LaunchEnv::from(std::collections::BTreeMap::from([
            ("Authorization".to_owned(), "Bearer token".to_owned()),
            ("Content-Type".to_owned(), "text/plain".to_owned()),
        ]));
        let mut channel = HttpChannel::new(
            "https://mcp.example.test",
            &headers,
            Box::new(Recording(Arc::clone(&sent))),
            1024,
        );
        assert!(matches!(
            channel.request("initialize", json!({})),
            Err(McpError::Auth(_))
        ));
        let sent = sent.lock().unwrap();
        assert!(sent.contains(&("authorization".to_owned(), "Bearer token".to_owned())));
        let content_types: Vec<_> = sent
            .iter()
            .filter(|(name, _)| name == "content-type")
            .collect();
        assert_eq!(content_types.len(), 1);
        assert_eq!(content_types[0].1, "application/json");
    }

    #[test]
    fn a_servers_log_never_shows_the_values_it_was_launched_with() {
        let env = LaunchEnv::from(std::collections::BTreeMap::from([
            (
                "DATABASE_URL".to_owned(),
                "postgres://user:hunter22@db/prod".to_owned(),
            ),
            ("DEBUG".to_owned(), "on".to_owned()),
        ]));
        let secrets = scrubbed_values(&env);
        assert_eq!(
            scrub(
                "connecting to postgres://user:hunter22@db/prod (debug on)",
                &secrets
            ),
            "connecting to [redacted] (debug on)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stdio_server_receives_its_launch_env() {
        let env = LaunchEnv::from(std::collections::BTreeMap::from([(
            "ARSY_MCP_LAUNCH_PROBE".to_owned(),
            "from-definition".to_owned(),
        )]));
        let reply = r#"read line; printf '{"jsonrpc":"2.0","id":1,"result":{"seen":"%s"}}\n' "$ARSY_MCP_LAUNCH_PROBE""#;
        let mut channel = StdioChannel::spawn(
            "probe",
            "sh",
            &["-c".to_owned(), reply.to_owned()],
            &env,
            Duration::from_secs(5),
            1024,
            &stderr_log_sink(),
        )
        .unwrap();
        let result = channel.request("probe", json!({})).unwrap();
        assert_eq!(result["seen"], "from-definition");
        let _ = channel.close();
    }

    /// A server's log lines reach the sink the caller supplied, named and
    /// scrubbed, and not this process's stderr — which is what an interactive
    /// session redraws over.
    #[test]
    fn a_servers_log_lines_go_to_the_sink_scrubbed() {
        let env = LaunchEnv::from(std::collections::BTreeMap::from([(
            "ARSY_MCP_TOKEN".to_owned(),
            "super-secret-value".to_owned(),
        )]));
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let captured = Arc::clone(&seen);
        let sink: McpLogSink = Arc::new(move |server: &str, line: &str| {
            captured
                .lock()
                .expect("the capture lock is never poisoned")
                .push(format!("{server}|{line}"));
        });
        // Logs to stderr, then answers one request so the channel is known to
        // have started before the assertion runs.
        let script = r#"printf 'launched with %s\n' "$ARSY_MCP_TOKEN" >&2; read line; printf '{"jsonrpc":"2.0","id":1,"result":{}}\n'"#;
        let mut channel = StdioChannel::spawn(
            "noisy",
            "sh",
            &["-c".to_owned(), script.to_owned()],
            &env,
            Duration::from_secs(5),
            1024,
            &sink,
        )
        .unwrap();
        channel.request("probe", json!({})).unwrap();
        let _ = channel.close();
        // The forwarder is its own thread, so the line may not have landed at
        // the moment the request returned.
        let deadline = Instant::now() + Duration::from_secs(5);
        let line = loop {
            if let Some(line) = seen
                .lock()
                .expect("the capture lock is never poisoned")
                .first()
                .cloned()
            {
                break line;
            }
            assert!(Instant::now() < deadline, "no log line reached the sink");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            line.starts_with("noisy|"),
            "the sink is told which server: {line}"
        );
        assert!(
            !line.contains("super-secret-value"),
            "the launch value is scrubbed before the sink sees it: {line}"
        );
    }

    #[test]
    fn an_unauthorized_server_error_is_not_retryable() {
        assert!(!McpError::Auth("expired".into()).is_retryable());
        assert!(!McpError::Disabled("docs".into()).is_retryable());
        assert!(!McpError::OutsideCeiling("x".into()).is_retryable());
        assert!(McpError::Timeout(Duration::from_secs(1)).is_retryable());
        assert_eq!(
            unwrap_result(&json!({"id": 1, "error": {"code": -32001, "message": "no session"}})),
            Err(McpError::Auth("no session".to_owned()))
        );
        assert_eq!(
            unwrap_result(&json!({"id": 1, "error": {"code": -32601, "message": "no method"}})),
            Err(McpError::Server {
                code: -32601,
                message: "no method".to_owned()
            })
        );
        assert_eq!(
            unwrap_result(&json!({"id": 1, "result": {"ok": true}})).unwrap()["ok"],
            true
        );
    }
}
