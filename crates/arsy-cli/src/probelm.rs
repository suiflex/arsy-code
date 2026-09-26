//! What the bundled `probelm` MCP connection knows about the configured
//! models, read before the first prompt so routing and the turn budget can use it.
//!
//! Two tools are read, never offered here: `list_models` is free metadata
//! (context window, vision) and refreshes hourly; `probe_models` sends real
//! completions, so it costs tokens and runs only for endpoints the operator
//! listed in `provider.health_probe`, at most every ten minutes. Both results
//! live in one user-scoped cache so one-shot `arsy run` processes share them.
//!
//! Everything probelm returns is untrusted: it may tie-break a route, shrink a
//! budget, or raise a warning, but never refuse or widen what a turn may do.

use arsy_code::mcp::{ChannelFactory, Connection, RealChannels};
use arsy_kernel::{
    config::{Config, PROBELM_MCP_SERVER},
    provider::ModelKey,
    pulse::{HealthChanged, HealthProbe, ProbeObservations, ProbeRecord, ProbeState},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

pub const CACHE_FILE: &str = "probelm-cache.json";
const CACHE_SCHEMA: u32 = 1;
/// `list_models` is free, so it refreshes hourly.
const SPECS_TTL_MS: u64 = 3_600_000;
/// `probe_models` costs tokens, so it runs at most every ten minutes.
const HEALTH_TTL_MS: u64 = 600_000;
const PROBE_JOBS: usize = 4;
/// Gateway error text is untrusted; cap it.
const ERROR_CHARS: usize = 200;

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireModels {
    models: Vec<WireModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireModel {
    id: String,
    capabilities: WireCapabilities,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireCapabilities {
    vision: bool,
    tools: bool,
    reasoning: bool,
    pdf: bool,
    context_window: Option<u64>,
    max_output: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireProbe {
    results: Vec<WireResult>,
    failures: Vec<WireFailure>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireResult {
    model: String,
    ping: Option<WirePing>,
    latency: Option<WireLatency>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WirePing {
    ok: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireLatency {
    ttft_secs: Option<f64>,
    tokens: u64,
    rate_per_sec: Option<f64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireFailure {
    model: String,
    error: String,
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    schema_version: u32,
    specs: SpecsSection,
    health: HealthSection,
}

#[derive(Default, Deserialize, Serialize)]
struct SpecsSection {
    attempted_at_ms: u64,
    fetched_at_ms: Option<u64>,
    models: Vec<ModelSpec>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelSpec {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub vision: bool,
    pub tools: bool,
    pub reasoning: bool,
    pub pdf: bool,
}

#[derive(Default, Deserialize, Serialize)]
struct HealthSection {
    attempted_at_ms: u64,
    models: Vec<HealthEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HealthEntry {
    pub provider: String,
    pub model: String,
    pub probelm_id: String,
    pub state: ProbeState,
    pub checked_at_ms: u64,
    pub ttft_secs: Option<f64>,
    pub rate_per_sec: Option<f64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InsightStatus {
    /// The connection is off, so nothing was asked.
    #[default]
    Disabled,
    Ready,
    /// On, but no model metadata is known.
    Unavailable,
}

/// What probelm said about the configured models, as of this process.
#[derive(Clone, Debug, Default)]
pub struct ModelInsight {
    pub status: InsightStatus,
    specs: Vec<ModelSpec>,
    specs_fetched_at_ms: Option<u64>,
    health: ProbeObservations,
    health_entries: Vec<HealthEntry>,
    health_probe: Vec<String>,
    /// Health transitions this process observed, to be recorded as events.
    pub transitions: Vec<HealthChanged>,
    /// Why the insight is incomplete, when it is.
    pub note: Option<String>,
}

impl ModelInsight {
    /// The one probelm model `model` names. Ambiguous or unknown is no data.
    pub fn spec(&self, model: &str) -> Option<&ModelSpec> {
        matched(&self.specs, model)
    }

    pub fn context_window(&self, model: &str) -> Option<u64> {
        self.spec(model)?.context_window
    }

    pub fn accepts_images(&self, model: &str) -> Option<bool> {
        self.spec(model).map(|spec| spec.vision)
    }

    pub fn report(&self) -> Value {
        json!({
            "status": self.status,
            "note": self.note,
            "health_probe": self.health_probe,
            "specs_fetched_at_ms": self.specs_fetched_at_ms,
            "models": self.specs,
            "health": self.health_entries.iter().map(|entry| json!({
                "provider": entry.provider,
                "model": entry.model,
                "state": entry.state,
                "checked_at_ms": entry.checked_at_ms,
                "ttft_secs": entry.ttft_secs,
                "rate_per_sec": entry.rate_per_sec,
                "error": entry.error,
            })).collect::<Vec<_>>(),
        })
    }
}

impl HealthProbe for ModelInsight {
    fn probe_state(&self, key: &ModelKey) -> ProbeState {
        self.health.probe_state(key)
    }

    fn probe_record(&self, key: &ModelKey) -> Option<ProbeRecord> {
        self.health.probe_record(key)
    }
}

/// ARSY-PRB-1000: probelm could not answer. The turn goes on without it.
pub(crate) fn unavailable(note: &str) -> crate::Diagnostic {
    crate::Diagnostic::warning(
        "ARSY-PRB-1000",
        note,
        "check it with `arsy mcp test probelm`, or turn it off with `arsy mcp disable probelm`",
    )
}

/// Read what probelm knows, refreshing the cache when it is stale.
///
/// `allow_probe` gates `probe_models`: only a caller that can record the
/// resulting transitions should spend tokens on it.
pub fn gather(config: &Config, allow_probe: bool) -> ModelInsight {
    let channels = RealChannels {
        http: || -> Box<dyn arsy_kernel::provider::wire::WireTransport> {
            Box::new(arsy_kernel::provider::http::HttpTransport::default())
        },
        // The TUI takes this path too, and stderr would corrupt its frame.
        // A failure surfaces through `note` instead.
        log: std::sync::Arc::new(|_: &str, _: &str| {}),
    };
    gather_with(
        config,
        allow_probe,
        arsy_kernel::artifact::unix_time_ms(),
        arsy_kernel::config::config_home().as_deref(),
        &channels,
    )
}

pub(crate) fn gather_with(
    config: &Config,
    allow_probe: bool,
    now_ms: u64,
    cache_home: Option<&Path>,
    channels: &dyn ChannelFactory,
) -> ModelInsight {
    let Some(server) = config
        .mcp_server(PROBELM_MCP_SERVER)
        .filter(|server| server.enabled)
    else {
        return ModelInsight::default();
    };
    let cache_path = cache_home.map(|home| home.join(CACHE_FILE));
    let mut cache = cache_path.as_deref().map(read_cache).unwrap_or_default();
    let opted = config.health_probe_endpoints();
    let targets = probe_targets(config);
    let need_specs = cache.specs.models.is_empty()
        || now_ms >= cache.specs.attempted_at_ms.saturating_add(SPECS_TTL_MS);
    let need_health = allow_probe
        && cache_home.is_some()
        && !targets.is_empty()
        && now_ms >= cache.health.attempted_at_ms.saturating_add(HEALTH_TTL_MS);
    if !(need_specs || need_health) {
        return insight_from(cache, &targets, opted, Vec::new(), None);
    }

    let (mut note, transitions) = match Connection::open(server, None, channels) {
        Err(error) => {
            cache.specs.attempted_at_ms = now_ms;
            if need_health {
                cache.health.attempted_at_ms = now_ms;
            }
            (Some(format!("probelm is unavailable: {error}")), Vec::new())
        }
        Ok(mut connection) => {
            let specs_note = need_specs
                .then(|| refresh_specs(&mut cache, &mut connection, now_ms))
                .flatten();
            let (health_note, transitions) = if need_health {
                refresh_health(&mut cache, &mut connection, &targets, now_ms)
            } else {
                (None, Vec::new())
            };
            let _ = connection.close();
            (health_note.or(specs_note), transitions)
        }
    };
    if let Some(path) = &cache_path {
        cache.schema_version = CACHE_SCHEMA;
        if let Err(error) = write_cache(path, &cache) {
            note.get_or_insert_with(|| format!("the probelm cache could not be written: {error}"));
        }
    }
    insight_from(cache, &targets, opted, transitions, note)
}

/// One key per model of every endpoint the operator opted in to probing.
fn probe_targets(config: &Config) -> Vec<ModelKey> {
    let opted = config.health_probe_endpoints();
    config
        .all_endpoints()
        .filter(|endpoint| opted.contains(&endpoint.id))
        .flat_map(|endpoint| {
            endpoint.models.iter().map(|model| ModelKey {
                provider: endpoint.id.clone(),
                model: model.clone(),
            })
        })
        .collect()
}

/// `list_models` into the cache. Returns why it failed, keeping the old specs.
fn refresh_specs(cache: &mut Cache, connection: &mut Connection, now_ms: u64) -> Option<String> {
    cache.specs.attempted_at_ms = now_ms;
    match call(connection, "list_models", json!({})).and_then(parse::<WireModels>) {
        Ok(listed) => {
            cache.specs.models = listed.models.into_iter().map(spec_of).collect();
            cache.specs.fetched_at_ms = Some(now_ms);
            None
        }
        Err(error) => Some(format!("probelm list_models failed: {error}")),
    }
}

/// `probe_models` for every target probelm lists, folded into the cache.
/// A target it does not list is skipped, and with none the window is left
/// alone so the next run tries again once specs are known.
fn refresh_health(
    cache: &mut Cache,
    connection: &mut Connection,
    targets: &[ModelKey],
    now_ms: u64,
) -> (Option<String>, Vec<HealthChanged>) {
    let matched_ids: Vec<(ModelKey, String)> = targets
        .iter()
        .filter_map(|key| {
            matched(&cache.specs.models, &key.model).map(|spec| (key.clone(), spec.id.clone()))
        })
        .collect();
    if matched_ids.is_empty() {
        return (None, Vec::new());
    }
    let mut ids: Vec<&str> = matched_ids.iter().map(|(_, id)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    cache.health.attempted_at_ms = now_ms;
    match call(
        connection,
        "probe_models",
        json!({"models": ids, "jobs": PROBE_JOBS}),
    )
    .and_then(parse::<WireProbe>)
    {
        Ok(probed) => (None, apply_probe(cache, &matched_ids, &probed, now_ms)),
        Err(error) => (
            Some(format!("probelm probe_models failed: {error}")),
            Vec::new(),
        ),
    }
}

/// The insight a cache supports. Health for an endpoint no longer opted in
/// is dropped, so it never penalizes a route.
fn insight_from(
    cache: Cache,
    targets: &[ModelKey],
    opted: &std::collections::BTreeSet<String>,
    transitions: Vec<HealthChanged>,
    note: Option<String>,
) -> ModelInsight {
    let health_entries: Vec<HealthEntry> = cache
        .health
        .models
        .into_iter()
        .filter(|entry| {
            targets
                .iter()
                .any(|key| key.provider == entry.provider && key.model == entry.model)
        })
        .collect();
    let mut health = ProbeObservations::new();
    for entry in &health_entries {
        health.observe(
            entry_key(entry),
            entry.state,
            entry.checked_at_ms,
            entry.error.clone(),
        );
    }
    ModelInsight {
        status: if cache.specs.models.is_empty() {
            InsightStatus::Unavailable
        } else {
            InsightStatus::Ready
        },
        specs: cache.specs.models,
        specs_fetched_at_ms: cache.specs.fetched_at_ms,
        health,
        health_entries,
        health_probe: opted.iter().cloned().collect(),
        transitions,
        note,
    }
}

/// Fold one `probe_models` answer into the cache and return the transitions
/// against what the cache held before.
fn apply_probe(
    cache: &mut Cache,
    matched_ids: &[(ModelKey, String)],
    probed: &WireProbe,
    now_ms: u64,
) -> Vec<HealthChanged> {
    let mut previous = ProbeObservations::new();
    for entry in &cache.health.models {
        previous.observe(
            entry_key(entry),
            entry.state,
            entry.checked_at_ms,
            entry.error.clone(),
        );
    }
    let entries: Vec<HealthEntry> = matched_ids
        .iter()
        .map(|(key, id)| {
            let result = probed.results.iter().find(|result| &result.model == id);
            let failure = probed.failures.iter().find(|failure| &failure.model == id);
            let latency = result.and_then(|result| result.latency.as_ref());
            HealthEntry {
                provider: key.provider.clone(),
                model: key.model.clone(),
                probelm_id: id.clone(),
                state: state_of(result, failure.is_some()),
                checked_at_ms: now_ms,
                ttft_secs: latency.and_then(|latency| latency.ttft_secs),
                rate_per_sec: latency.and_then(|latency| latency.rate_per_sec),
                error: failure.map(|failure| capped(&failure.error)),
            }
        })
        .collect();
    let changes = previous.replace_all(
        entries.iter().map(|entry| {
            (
                entry_key(entry),
                entry.state,
                entry.checked_at_ms,
                entry.error.clone(),
            )
        }),
        now_ms,
    );
    cache.health.models = entries;
    changes
}

/// Mirrors probelm's own verdict for one probed model.
fn state_of(result: Option<&WireResult>, failed: bool) -> ProbeState {
    if failed {
        return ProbeState::Unreachable;
    }
    let Some(ping) = result.and_then(|result| result.ping.as_ref()) else {
        return ProbeState::Unknown;
    };
    let latency = result.and_then(|result| result.latency.as_ref());
    match ping.ok {
        false => ProbeState::Unreachable,
        true if latency
            .is_some_and(|latency| latency.tokens == 0 && latency.rate_per_sec.is_none()) =>
        {
            ProbeState::Degraded
        }
        true => ProbeState::Healthy,
    }
}

fn spec_of(model: WireModel) -> ModelSpec {
    let caps = model.capabilities;
    // A zero window is probelm saying "unknown"; read as a size it would
    // leave a turn no budget at all.
    ModelSpec {
        id: model.id,
        context_window: caps.context_window.filter(|&tokens| tokens > 0),
        max_output: caps.max_output.filter(|&tokens| tokens > 0),
        vision: caps.vision,
        tools: caps.tools,
        reasoning: caps.reasoning,
        pdf: caps.pdf,
    }
}

fn entry_key(entry: &HealthEntry) -> ModelKey {
    ModelKey {
        provider: entry.provider.clone(),
        model: entry.model.clone(),
    }
}

/// 0: the same id. 1: a gateway id that routes to it, `<prefix>/<model>`.
fn match_rank(probelm_id: &str, model: &str) -> Option<u8> {
    let id = probelm_id.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    if id == model {
        Some(0)
    } else if id.ends_with(&format!("/{model}")) {
        Some(1)
    } else {
        None
    }
}

/// The single best match; two at the best rank is ambiguous, so none.
fn matched<'a>(specs: &'a [ModelSpec], model: &str) -> Option<&'a ModelSpec> {
    for rank in 0..=1 {
        let mut hits = specs
            .iter()
            .filter(|spec| match_rank(&spec.id, model) == Some(rank));
        match (hits.next(), hits.next()) {
            (Some(spec), None) => return Some(spec),
            (Some(_), Some(_)) => return None,
            (None, _) => {}
        }
    }
    None
}

fn call(connection: &mut Connection, tool: &str, arguments: Value) -> Result<Value, String> {
    connection
        .call_tool(tool, arguments)
        .map_err(|error| capped(&error.to_string()))
        .and_then(tool_json)
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// The JSON a `tools/call` result carries, or the tool's own error text.
fn tool_json(result: Value) -> Result<Value, String> {
    let text = result["content"][0]["text"].as_str();
    if result["isError"] == Value::Bool(true) {
        return Err(capped(text.unwrap_or("probelm reported an error")));
    }
    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(structured.clone());
    }
    text.and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| "probelm returned no JSON".to_owned())
}

fn capped(text: &str) -> String {
    text.chars().take(ERROR_CHARS).collect()
}

fn read_cache(path: &Path) -> Cache {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| cache.schema_version == CACHE_SCHEMA)
        .unwrap_or_default()
}

/// Temp file then rename, so a concurrent reader never sees half a cache.
fn write_cache(path: &Path, cache: &Cache) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{CACHE_FILE}.{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(cache)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_code::mcp::{Channel, McpError};
    use arsy_kernel::config::{Layer, McpServer};
    use std::sync::{Arc, Mutex};

    const MINUTE: u64 = 60_000;

    /// What the fake probelm answers, and what it was asked.
    #[derive(Default)]
    struct Script {
        list: Value,
        probe: Value,
        opened: usize,
        called: Vec<String>,
        probed: Vec<Value>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Script>>);

    impl Fake {
        fn script(&self) -> std::sync::MutexGuard<'_, Script> {
            self.0.lock().unwrap()
        }
    }

    impl Channel for Fake {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
            let mut script = self.script();
            Ok(match method {
                "initialize" => json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "probelm", "version": "1.1.0"},
                }),
                "tools/list" => json!({"tools": [
                    {"name": "list_models", "inputSchema": {"type": "object"}},
                    {"name": "probe_models", "inputSchema": {"type": "object"}},
                ]}),
                "tools/call" => {
                    let name = params["name"].as_str().unwrap_or_default().to_owned();
                    script.called.push(name.clone());
                    if name == "probe_models" {
                        script.probed.push(params["arguments"].clone());
                        script.probe.clone()
                    } else {
                        script.list.clone()
                    }
                }
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

    impl ChannelFactory for Fake {
        fn connect(&self, _definition: &McpServer) -> Result<Box<dyn Channel>, McpError> {
            self.script().opened += 1;
            Ok(Box::new(self.clone()))
        }
    }

    fn answer(value: Value) -> Value {
        json!({"content": [{"type": "text", "text": value.to_string()}], "structuredContent": value})
    }

    fn listed(models: &[(&str, u64)]) -> Value {
        answer(json!({"models": models.iter().map(|(id, window)| json!({
            "id": id,
            "capabilities": {"vision": false, "contextWindow": window, "maxOutput": null},
        })).collect::<Vec<_>>()}))
    }

    fn probed(healthy: &[&str], failing: &[&str]) -> Value {
        answer(json!({
            "results": healthy.iter().map(|id| json!({
                "model": id,
                "ping": {"ok": true, "http_code": 200},
                "latency": {"ttft_secs": 0.2, "total_secs": 0.3, "tokens": 4, "rate_per_sec": 20.0},
            })).collect::<Vec<_>>(),
            "failures": failing.iter().map(|id| json!({"model": id, "error": "HTTP 503"})).collect::<Vec<_>>(),
        }))
    }

    fn config(directory: &Path, probelm: bool, health_probe: &[&str]) -> Config {
        let path = directory.join("arsy.json");
        let body = json!({
            "schema_version": 1,
            "mcp": {"server": {"probelm": {"enabled": probelm}}},
            "provider": {
                "health_probe": health_probe,
                "endpoint": {"gw": {
                    "kind": "openai",
                    "base_url": "http://127.0.0.1:9/v1",
                    "model": "tiny",
                    "models": ["tiny", "big"],
                }},
            },
        });
        std::fs::write(&path, body.to_string()).unwrap();
        Config::load(&[(Layer::User, path)]).unwrap()
    }

    fn states(insight: &ModelInsight) -> Vec<(String, ProbeState)> {
        ["tiny", "big"]
            .iter()
            .map(|model| {
                let key = ModelKey {
                    provider: "gw".into(),
                    model: (*model).into(),
                };
                ((*model).to_owned(), insight.probe_state(&key))
            })
            .collect()
    }

    #[test]
    fn a_model_matches_one_gateway_id_or_none() {
        let spec = |id: &str| ModelSpec {
            id: id.to_owned(),
            context_window: Some(1_048_576),
            ..ModelSpec::default()
        };
        let insight = ModelInsight {
            specs: vec![spec("gw/glm-5.2"), spec("a/m"), spec("b/m")],
            ..ModelInsight::default()
        };
        assert_eq!(insight.context_window("GLM-5.2"), Some(1_048_576));
        assert_eq!(insight.spec("m"), None, "two gateways serve `m`");
        assert_eq!(insight.spec("glm"), None);

        let exact = ModelInsight {
            specs: vec![spec("m"), spec("a/m")],
            ..ModelInsight::default()
        };
        assert_eq!(
            exact.spec("m").unwrap().id,
            "m",
            "the same id outranks a suffix"
        );
    }

    #[test]
    fn nothing_is_probed_unless_the_endpoint_opted_in() {
        let directory = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        fake.script().list = listed(&[("gw/tiny", 8_000), ("gw/big", 200_000)]);
        let config = config(directory.path(), true, &[]);

        let insight = gather_with(&config, true, 10 * MINUTE, Some(directory.path()), &fake);
        assert_eq!(insight.status, InsightStatus::Ready);
        assert_eq!(insight.context_window("big"), Some(200_000));
        assert_eq!(fake.script().called, ["list_models"]);
        assert!(insight.transitions.is_empty());
    }

    #[test]
    fn health_is_probed_once_per_window_and_only_changes_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        fake.script().list = listed(&[("gw/tiny", 8_000), ("gw/big", 200_000)]);
        fake.script().probe = probed(&["gw/big"], &["gw/tiny"]);
        let config = config(directory.path(), true, &["gw"]);
        let start = 100 * MINUTE;

        let first = gather_with(&config, true, start, Some(directory.path()), &fake);
        assert_eq!(
            fake.script().probed,
            [json!({"models": ["gw/big", "gw/tiny"], "jobs": PROBE_JOBS})]
        );
        let moves: Vec<_> = first
            .transitions
            .iter()
            .map(|change| (change.model.as_str(), change.from, change.to))
            .collect();
        assert_eq!(
            moves,
            [
                ("tiny", ProbeState::Unknown, ProbeState::Unreachable),
                ("big", ProbeState::Unknown, ProbeState::Healthy),
            ]
        );
        assert_eq!(
            states(&first),
            [
                ("tiny".to_owned(), ProbeState::Unreachable),
                ("big".to_owned(), ProbeState::Healthy)
            ]
        );
        assert_eq!(first.report()["health"][0]["error"], "HTTP 503");

        // A minute later, a new process reads the cache and asks nothing.
        let opened = fake.script().opened;
        let cached = gather_with(&config, true, start + MINUTE, Some(directory.path()), &fake);
        assert_eq!(fake.script().opened, opened);
        assert!(cached.transitions.is_empty());
        assert_eq!(states(&cached), states(&first));

        // Past the window the probe runs again, and only the change is reported.
        fake.script().probe = probed(&["gw/big", "gw/tiny"], &[]);
        let later = gather_with(
            &config,
            true,
            start + 11 * MINUTE,
            Some(directory.path()),
            &fake,
        );
        let moves: Vec<_> = later
            .transitions
            .iter()
            .map(|change| (change.model.as_str(), change.from, change.to))
            .collect();
        assert_eq!(
            moves,
            [("tiny", ProbeState::Unreachable, ProbeState::Healthy)]
        );
    }

    #[test]
    fn a_failed_probe_keeps_the_last_health_and_reports_no_change() {
        let directory = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        fake.script().list = listed(&[("gw/tiny", 8_000), ("gw/big", 200_000)]);
        fake.script().probe = probed(&["gw/big"], &["gw/tiny"]);
        let config = config(directory.path(), true, &["gw"]);
        let first = gather_with(&config, true, 100 * MINUTE, Some(directory.path()), &fake);

        fake.script().probe =
            json!({"isError": true, "content": [{"type": "text", "text": "gateway down"}]});
        let failed = gather_with(&config, true, 120 * MINUTE, Some(directory.path()), &fake);
        assert!(
            failed
                .note
                .as_deref()
                .is_some_and(|note| note.contains("probelm probe_models failed")),
            "{:?}",
            failed.note
        );
        assert!(failed.transitions.is_empty());
        assert_eq!(states(&failed), states(&first));
    }

    #[test]
    fn a_disabled_connection_asks_nothing_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let config = config(directory.path(), false, &["gw"]);
        let insight = gather_with(&config, true, MINUTE, Some(directory.path()), &fake);
        assert_eq!(insight.status, InsightStatus::Disabled);
        assert_eq!(fake.script().opened, 0);
        assert!(!directory.path().join(CACHE_FILE).exists());
    }
}
