//! MCP connections held for a whole session.
//!
//! Connecting at every turn restarted every server each time — an `npx`
//! package reinstalling, a database handshake repeated — and made the slowest
//! server the floor on every turn. A session holds one [`McpConnector`]
//! instead, and at each turn boundary brings it in line with configuration:
//!
//! - a server that is new, or whose definition changed, connects on its own
//!   thread, so no turn waits for it;
//! - a server that was switched off or removed is disconnected;
//! - a server that is up is reused, and one whose connection broke under a
//!   call is reopened — at most once per turn, which is the retry bound;
//! - a server that failed to connect is not retried until its definition
//!   changes, so a missing binary is reported once rather than every turn.
//!
//! While a server is still connecting, its tools are offered from what it
//! published the last time it connected under the same definition, kept in
//! `mcp-tools.json` beside the operator's configuration. A call to one of
//! those waits for the connection (see `mcpops::Pending`). The cache is keyed by
//! a SHA-256 of the definition, launch values included, so a changed token or
//! command never serves stale tools, and the file holds only the digest.

use arsy_code::{
    agent::{
        mcpops::{self, Connections, Pending},
        DynamicTool,
    },
    mcp::{ChannelFactory, Connection},
};
use arsy_kernel::config::{Config, McpServer, McpTransport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// How long cached tools are offered for a server that has not connected.
const CACHE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// How many unshown server log lines are held at once.
const MAX_HELD_LOG_LINES: usize = 512;

type Channels = Arc<dyn ChannelFactory + Send + Sync>;

pub struct McpConnector {
    connections: Connections,
    pending: Pending,
    /// The definition each server was last started under.
    started: Arc<Mutex<BTreeMap<String, String>>>,
    /// Servers whose last attempt under that definition failed.
    failed: Arc<Mutex<BTreeMap<String, String>>>,
    /// Failures not yet reported.
    failures: Arc<Mutex<Vec<String>>>,
    /// Server log lines not yet shown, oldest first, in the order they were
    /// written. Held rather than printed because the forwarding threads run
    /// while a frame is being painted; draining them at a known point in the
    /// loop is what keeps a noisy server out of the middle of the composer.
    logs: Arc<Mutex<Vec<(String, String)>>>,
    cache: Option<PathBuf>,
    /// One writer at a time for the cache file.
    cache_lock: Arc<Mutex<()>>,
    channels: Channels,
}

impl McpConnector {
    /// Real servers, with tools cached at `cache` when there is one.
    pub fn new(cache: Option<PathBuf>) -> Self {
        let logs: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let captured = Arc::clone(&logs);
        let connector = Self::with_channels(
            cache,
            Arc::new(arsy_code::mcp::RealChannels {
                http: || -> Box<dyn arsy_kernel::provider::wire::WireTransport> {
                    Box::new(arsy_kernel::provider::http::HttpTransport::default())
                },
                // Bounded: a server stuck in a log loop must not grow this
                // without limit between two drains. The oldest lines go, and
                // the count the summary reports still counts them.
                log: Arc::new(move |server: &str, line: &str| {
                    if let Ok(mut held) = captured.lock() {
                        if held.len() >= MAX_HELD_LOG_LINES {
                            held.remove(0);
                        }
                        held.push((server.to_owned(), line.to_owned()));
                    }
                }),
            }),
        );
        Self { logs, ..connector }
    }

    pub fn with_channels(cache: Option<PathBuf>, channels: Channels) -> Self {
        Self {
            connections: Arc::default(),
            pending: Pending::default(),
            started: Arc::default(),
            failed: Arc::default(),
            failures: Arc::default(),
            logs: Arc::default(),
            cache,
            cache_lock: Arc::default(),
            channels,
        }
    }

    /// What a turn's runtime reaches servers through.
    pub fn session(&self) -> (Connections, Pending) {
        (Arc::clone(&self.connections), self.pending.clone())
    }

    /// Bring the live set in line with `config`, and return the tools the
    /// model may be offered now.
    pub fn sync(&self, config: &Config) -> Vec<DynamicTool> {
        let enabled: Vec<(&McpServer, String)> = config
            .mcp_servers()
            .filter(|server| server.enabled)
            .map(|server| (server, fingerprint(server)))
            .collect();
        self.retire(&enabled);
        for (server, digest) in &enabled {
            self.ensure(server, digest);
        }
        enabled
            .iter()
            .flat_map(|(server, digest)| self.tools_for(&server.name, digest))
            .collect()
    }

    /// Server log lines since the last call, each shown once.
    pub fn logs(&self) -> Vec<(String, String)> {
        self.logs
            .lock()
            .map(|mut logs| std::mem::take(&mut *logs))
            .unwrap_or_default()
    }

    /// Failures since the last call, each reported once.
    pub fn failures(&self) -> Vec<String> {
        self.failures
            .lock()
            .map(|mut failures| std::mem::take(&mut *failures))
            .unwrap_or_default()
    }

    /// Disconnect whatever is no longer enabled, or no longer defined the way
    /// it was when it connected.
    fn retire(&self, enabled: &[(&McpServer, String)]) {
        let current = |name: &str| {
            enabled
                .iter()
                .find(|(server, _)| server.name == name)
                .map(|(_, digest)| digest.as_str())
        };
        let Ok(mut started) = self.started.lock() else {
            return;
        };
        let stale: Vec<String> = started
            .iter()
            .filter(|(name, digest)| current(name) != Some(digest.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            started.remove(&name);
            if let Ok(mut failed) = self.failed.lock() {
                failed.remove(&name);
            }
            // Dropping a connection closes it.
            if let Ok(mut connections) = self.connections.lock() {
                connections.remove(&name);
            }
        }
    }

    /// Start a server unless it is up, connecting, or failed as defined.
    fn ensure(&self, server: &McpServer, digest: &str) {
        let is = |map: &Mutex<BTreeMap<String, String>>| {
            map.lock()
                .is_ok_and(|map| map.get(&server.name).is_some_and(|known| known == digest))
        };
        let live = self
            .connections
            .lock()
            .is_ok_and(|connections| connections.contains_key(&server.name));
        if is(&self.failed) || (is(&self.started) && (live || self.pending.contains(&server.name)))
        {
            return;
        }
        if let Ok(mut started) = self.started.lock() {
            started.insert(server.name.clone(), digest.to_owned());
        }
        self.pending.start(&server.name);
        self.spawn(server.clone(), digest.to_owned());
    }

    fn spawn(&self, server: McpServer, digest: String) {
        let connections = Arc::clone(&self.connections);
        let pending = self.pending.clone();
        let started = Arc::clone(&self.started);
        let failed = Arc::clone(&self.failed);
        let failures = Arc::clone(&self.failures);
        let channels = Arc::clone(&self.channels);
        let cache = self.cache.clone();
        let cache_lock = Arc::clone(&self.cache_lock);
        std::thread::spawn(move || {
            let name = server.name.clone();
            match Connection::open(&server, None, channels.as_ref()) {
                // Kept only if the definition is still the one it was started
                // under: a toggle made while it connected wins.
                Ok(connection) if is_current(&started, &name, &digest) => {
                    if let Some(path) = &cache {
                        let _guard = cache_lock.lock();
                        store(path, &name, &digest, &mcpops::tools_of(&connection));
                    }
                    if let Ok(mut connections) = connections.lock() {
                        connections.insert(name.clone(), connection);
                    }
                }
                // A superseded attempt reports nothing: its failure is not
                // the current definition's.
                Ok(_) => {}
                Err(_) if !is_current(&started, &name, &digest) => {}
                Err(error) => {
                    if let Ok(mut failed) = failed.lock() {
                        failed.insert(name.clone(), digest);
                    }
                    if let Ok(mut failures) = failures.lock() {
                        failures.push(format!("MCP server `{name}` is unavailable: {error}"));
                    }
                }
            }
            pending.finish(&name);
        });
    }

    /// The tools of a connected server, or, while it connects, the ones it
    /// published last time under the same definition.
    fn tools_for(&self, name: &str, digest: &str) -> Vec<DynamicTool> {
        if let Some(tools) = self
            .connections
            .lock()
            .ok()
            .and_then(|connections| connections.get(name).map(mcpops::tools_of))
        {
            return tools;
        }
        match (&self.cache, self.pending.contains(name)) {
            (Some(path), true) => cached(path, name, digest),
            _ => Vec::new(),
        }
    }
}

fn is_current(started: &Mutex<BTreeMap<String, String>>, name: &str, digest: &str) -> bool {
    started
        .lock()
        .is_ok_and(|started| started.get(name).is_some_and(|known| known == digest))
}

/// A digest of everything that decides what a connection reaches.
fn fingerprint(server: &McpServer) -> String {
    let mut hasher = Sha256::new();
    let mut feed = |part: &str| {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    };
    feed(&server.name);
    feed(server.transport.kind());
    // Each part fed on its own: the display target joins command and
    // arguments with spaces, so `a b` + `c` and `a` + `b c` would collide.
    let launch = match &server.transport {
        McpTransport::Stdio { command, args, env } => {
            feed(command);
            feed(&args.len().to_string());
            for arg in args {
                feed(arg);
            }
            env
        }
        McpTransport::Http { url, headers } => {
            feed(url);
            headers
        }
    };
    for (key, value) in launch.iter() {
        feed(key);
        feed(value);
    }
    feed(&server.timeout_ms.to_string());
    feed(&server.max_body_bytes.to_string());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_ms() -> u64 {
    arsy_kernel::artifact::unix_time_ms()
}

fn read_cache(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("servers").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn store(path: &Path, name: &str, digest: &str, tools: &[DynamicTool]) {
    let mut servers = read_cache(path);
    servers.insert(
        name.to_owned(),
        json!({
            "fingerprint": digest,
            "saved_ms": now_ms(),
            "tools": tools.iter().map(|tool| json!({
                "name": tool.name,
                "tool": tool.tool,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })).collect::<Vec<_>>(),
        }),
    );
    let body = json!({"version": 1, "servers": servers});
    // A cache that cannot be written only costs the next session its head
    // start, so a failure here is not reported.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body.to_string());
}

fn cached(path: &Path, name: &str, digest: &str) -> Vec<DynamicTool> {
    let servers = read_cache(path);
    let Some(entry) = servers
        .get(name)
        .filter(|entry| entry["fingerprint"] == digest)
        .filter(|entry| {
            entry["saved_ms"]
                .as_u64()
                .is_some_and(|saved| now_ms().saturating_sub(saved) < CACHE_TTL_MS)
        })
    else {
        return Vec::new();
    };
    entry["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            Some(DynamicTool {
                name: tool["name"].as_str()?.to_owned(),
                description: tool["description"].as_str().unwrap_or_default().to_owned(),
                input_schema: tool["input_schema"].clone(),
                operation: "mcp.call",
                server: name.to_owned(),
                tool: tool["tool"].as_str()?.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
