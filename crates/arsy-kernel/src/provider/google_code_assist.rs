//! Google Cloud Code Assist adapter, as Antigravity speaks it.
//!
//! Not Chat Completions and not Anthropic Messages: a Gemini
//! `GenerateContentRequest` (`contents` of `parts`, `systemInstruction` as an
//! object, tools as `functionDeclarations`) wrapped in a Code Assist envelope
//! that names the model and a Google Cloud project. The project is discovered
//! once with `:loadCodeAssist` — provisioning the free tier with
//! `:onboardUser` first when the account has no tier yet — and cached.
//!
//! The response is Gemini-shaped too: SSE lines of
//! `{"response":{"candidates":[{"content":{"parts":[…]},"finishReason":…}]}}`.
//! A `functionCall` part arrives whole, so a tool call is started and completed
//! from the same part.

use super::{
    wire::{ApiKey, WireRequest, WireResponse, WireTransport},
    CanonicalModelRequest, Effort, ModelContent, ModelEvent, ModelEventStream, ModelMessage,
    ModelProvider, ModelRole, ProviderDescriptor, ProviderError, StopReason, ToolSchema,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::{collections::VecDeque, sync::Mutex, time::Duration};

/// Tool names on the Google Code Assist / Antigravity wire must match
/// `^[a-zA-Z0-9_-]{1,128}$`, the same pattern Anthropic enforces. ARSY's
/// canonical names are dotted (`fs.read`, `search.text`, ...), so they need
/// to be sanitized before they are sent and mapped back when the reply arrives.
fn wire_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub const DEFAULT_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";
/// The Antigravity release the user agent claims when nothing overrides it.
const ANTIGRAVITY_VERSION: &str = "2.8.0";
/// Names a newer Antigravity release without a rebuild.
pub const ANTIGRAVITY_VERSION_VAR: &str = "ARSY_ANTIGRAVITY_VERSION";

/// User agent expected by the Cloud Code Assist endpoint for Antigravity.
///
/// The backend offers newer models only to clients that claim a recent
/// enough release, so a pinned version slowly hides models. `ARSY_ANTIGRAVITY_VERSION`
/// moves it forward without waiting for an ARSY release.
///
/// ponytail: pinned plus an override; read the version from Antigravity's
/// update manifest if keeping it current by hand becomes a chore.
pub fn antigravity_user_agent() -> String {
    let version = std::env::var(ANTIGRAVITY_VERSION_VAR)
        .ok()
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| ANTIGRAVITY_VERSION.to_owned());
    format!(
        "antigravity/hub/{} (aidev_client; os_type=darwin; arch=arm64; cl=963137146)",
        version.trim()
    )
}

pub struct GoogleCodeAssistProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    transport: T,
    redactor: Redactor,
    /// Resolved lazily on the first `stream` and reused after. `None` means not
    /// looked up yet, or looked up and failed, so the next turn asks again.
    project: Mutex<Option<String>>,
    /// Between two polls of a free-tier provisioning still in progress.
    onboard_poll: Duration,
}

/// The tier Antigravity provisions for a personal Google account.
const FREE_TIER_ID: &str = "free-tier";
/// Polls of `:onboardUser` before giving up; with the default interval this
/// is the 30 seconds the reference clients allow.
const ONBOARD_POLLS: u32 = 15;

impl<T: WireTransport> GoogleCodeAssistProvider<T> {
    pub fn new(key: ApiKey, transport: T) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key, transport)
    }

    pub fn with_base_url(base_url: impl Into<String>, key: ApiKey, transport: T) -> Self {
        Self {
            descriptor: ProviderDescriptor {
                id: "google_code_assist".to_owned(),
                max_retries: 3,
            },
            base_url: base_url.into(),
            key,
            transport,
            redactor: Redactor::new(),
            project: Mutex::new(None),
            onboard_poll: Duration::from_secs(2),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.descriptor.id = id.into();
        self
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Pin the project rather than discovering it: the one a login already
    /// discovered and stored with its tokens.
    pub fn with_project(self, project: impl Into<String>) -> Self {
        *self.project.lock().unwrap() = Some(project.into());
        self
    }

    /// Poll a provisioning in progress this often. Tests set zero.
    pub fn with_onboard_poll(mut self, interval: Duration) -> Self {
        self.onboard_poll = interval;
        self
    }

    fn action_url(&self, action: &str) -> String {
        format!(
            "{}/{API_VERSION}:{action}",
            self.base_url.trim_end_matches('/')
        )
    }

    fn headers(&self, _project: &str, streaming: bool) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.key.expose()),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("user-agent".to_owned(), antigravity_user_agent()),
        ];
        if streaming {
            headers.push(("accept".to_owned(), "text/event-stream".to_owned()));
        }
        headers
    }

    /// The project to send, discovering it once. A discovery that fails is
    /// not cached, and fails the turn with the reason: a request sent with no
    /// project is refused by Code Assist with a bare 403 that says nothing of
    /// why, which is what an unprovisioned account used to see.
    fn project(&self) -> Result<String, ProviderError> {
        let mut cached = self.project.lock().unwrap();
        if let Some(project) = cached.as_ref() {
            return Ok(project.clone());
        }
        let discovered = self.discover_project()?;
        *cached = Some(discovered.clone());
        Ok(discovered)
    }

    /// Find the Code Assist project this account works in, provisioning the
    /// free tier first when the account has none.
    ///
    /// An account Google has not provisioned yet still gets a 200 from
    /// `:loadCodeAssist` — with no `currentTier` and no project — so whether
    /// to onboard is decided by that field, not by an error status. When the
    /// free tier is refused (an account that needs verification, an
    /// unsupported region), Google says why and where to fix it, and that is
    /// passed on rather than replaced by the 403 the turn would get later.
    pub fn discover_project(&self) -> Result<String, ProviderError> {
        let mut status = self.load_code_assist()?;
        if let Some(project) = project_id(&status) {
            return Ok(project);
        }
        if status.get("currentTier").is_none_or(Value::is_null) {
            // Not `Auth`: signing in again cannot fix an account Google wants
            // verified, and an auth failure is answered by asking for that.
            if let Some(refusal) = free_tier_refusal(&status) {
                return Err(ProviderError::InvalidRequest(refusal));
            }
            self.onboard()?;
            status = self.load_code_assist()?;
        }
        project_id(&status).ok_or_else(|| {
            ProviderError::Auth(
                "Code Assist did not name a project for this account; open Antigravity once \
                 with it, then sign in again"
                    .to_owned(),
            )
        })
    }

    fn control(&self, action: &str, body: Value) -> Result<Value, ProviderError> {
        let response = self.transport.send(WireRequest {
            url: self.action_url(action),
            headers: self.headers("", false),
            body: body.to_string(),
        })?;
        if response.status != 200 {
            return Err(normalize_status(response));
        }
        let body: String = response.lines.filter_map(Result::ok).collect();
        serde_json::from_str(&body)
            .map_err(|error| ProviderError::Decode(format!("{action}: {error}")))
    }

    fn load_code_assist(&self) -> Result<Value, ProviderError> {
        self.control(
            "loadCodeAssist",
            json!({ "metadata": { "ideType": "ANTIGRAVITY" } }),
        )
    }

    /// Provision the free tier and wait for it. `:onboardUser` answers with a
    /// long-running operation; asking again returns the same operation's
    /// progress, which is how the Gemini CLI waits for it too.
    fn onboard(&self) -> Result<(), ProviderError> {
        let request = json!({
            "tierId": FREE_TIER_ID,
            "metadata": { "ideType": "ANTIGRAVITY" },
        });
        for poll in 0..ONBOARD_POLLS {
            if poll > 0 {
                std::thread::sleep(self.onboard_poll);
            }
            let operation = self.control("onboardUser", request.clone())?;
            if operation.get("done").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if let Some(error) = operation.get("error").filter(|error| !error.is_null()) {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(ProviderError::InvalidRequest(format!(
                    "provisioning the Antigravity free tier failed: {message}"
                )));
            }
            return Ok(());
        }
        // Not a transport error: those are retried, and each retry would
        // wait out another full provisioning window inside one turn. Not an
        // auth failure either: a fresh login would not make it any faster.
        Err(ProviderError::InvalidRequest(
            "provisioning the Antigravity free tier did not finish in time; try again".to_owned(),
        ))
    }

    pub fn fetch_available_models(&self) -> Option<Vec<String>> {
        let request = WireRequest {
            url: self.action_url("fetchAvailableModels"),
            headers: self.headers("", false),
            body: "{}".to_owned(),
        };
        let response = self.transport.send(request).ok()?;
        if response.status != 200 {
            return None;
        }
        let body: String = response.lines.filter_map(Result::ok).collect();
        let value: Value = serde_json::from_str(&body).ok()?;
        let models_map = value.get("models")?.as_object()?;
        let mut list: Vec<String> = models_map.keys().cloned().collect();
        list.sort();
        Some(list)
    }

    pub fn encode(&self, request: &CanonicalModelRequest, project: &str) -> WireRequest {
        let mut generation = Map::new();
        generation.insert(
            "maxOutputTokens".to_owned(),
            json!(request.max_output_tokens),
        );
        if let Some(effort) = request.effort {
            let (numerator, denominator) = effort.thinking_share();
            let budget = request
                .max_output_tokens
                .saturating_mul(numerator)
                .saturating_div(denominator.max(1));
            generation.insert(
                "thinkingConfig".to_owned(),
                json!({"thinkingBudget": budget, "includeThoughts": true}),
            );
        }

        // Gemini keys a `functionResponse` by the function's name, but a
        // canonical tool result carries only the call id. Recover the name from
        // the `functionCall` earlier in the conversation that shares that id.
        let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for message in &request.messages {
            for content in &message.content {
                if let ModelContent::ToolCall { id, name, .. } = content {
                    names.insert(id.as_str(), name.as_str());
                }
            }
        }

        let mut inner = Map::new();
        inner.insert(
            "contents".to_owned(),
            Value::Array(
                request
                    .messages
                    .iter()
                    .flat_map(|message| encode_message(message, &names))
                    .collect(),
            ),
        );
        if let Some(system) = request
            .system
            .as_deref()
            .filter(|system| !system.trim().is_empty())
        {
            inner.insert(
                "systemInstruction".to_owned(),
                json!({
                    "role": "user",
                    "parts": [{"text": system}]
                }),
            );
        }
        inner.insert("generationConfig".to_owned(), Value::Object(generation));
        if !request.tools.is_empty() {
            inner.insert(
                "tools".to_owned(),
                json!([{
                    "functionDeclarations": request
                        .tools
                        .iter()
                        .map(encode_tool)
                        .collect::<Vec<_>>(),
                }]),
            );
            inner.insert(
                "toolConfig".to_owned(),
                json!({
                    "functionCallingConfig": {
                        "mode": "VALIDATED"
                    }
                }),
            );
        }
        let wire_model = routed_wire_model(&request.model.model, request.effort);
        let is_claude = wire_model.contains("claude");
        let mut labels = Map::new();
        labels.insert(
            "used_claude".to_owned(),
            json!(if is_claude { "true" } else { "false" }),
        );
        labels.insert(
            "used_claude_conservative".to_owned(),
            json!(if is_claude { "true" } else { "false" }),
        );
        inner.insert("labels".to_owned(), Value::Object(labels));
        let hash = request
            .idempotency_key
            .as_str()
            .bytes()
            .fold(0i64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as i64));
        inner.insert("sessionId".to_owned(), json!(hash.to_string()));

        let mut envelope = Map::new();
        envelope.insert("model".to_owned(), json!(wire_model));
        if !project.is_empty() {
            envelope.insert("project".to_owned(), json!(project));
        }
        envelope.insert("request".to_owned(), Value::Object(inner));
        envelope.insert("userAgent".to_owned(), json!("antigravity"));
        envelope.insert("requestType".to_owned(), json!("agent"));
        let step_id = format!(
            "agent/arsy/{}/{}",
            crate::artifact::unix_time_ms(),
            request.idempotency_key.as_str()
        );
        envelope.insert("requestId".to_owned(), json!(step_id));

        WireRequest {
            url: format!("{}?alt=sse", self.action_url("streamGenerateContent")),
            headers: self.headers(project, true),
            body: Value::Object(envelope).to_string(),
        }
    }
}

/// The model the API is asked for: flash carries the effort as a suffix, Pro
/// maps its levels onto two ids, a Claude model asked to think names the
/// thinking variant, and anything unrecognized passes through untouched
/// (including the empty model, which is never rewritten).
fn routed_wire_model(model: &str, effort: Option<Effort>) -> &str {
    let thinking = matches!(
        effort,
        Some(Effort::Medium | Effort::High | Effort::XHigh | Effort::Max)
    );
    match model {
        "gemini-3.8-flash" => gemini_3_8_flash(effort),
        "gemini-3.7-flash" => gemini_3_7_flash(effort),
        "gemini-3.1-pro" => gemini_3_1_pro(effort),
        "claude-3-7-sonnet" if thinking => "claude-3-7-sonnet-thinking",
        "claude-sonnet-4-5" if thinking => "claude-sonnet-4-5-thinking",
        "claude-sonnet-4-6" if thinking => "claude-sonnet-4-6-thinking",
        "claude-opus-4-5" if thinking => "claude-opus-4-5-thinking",
        "claude-opus-4-6" if thinking => "claude-opus-4-6-thinking",
        other => other,
    }
}

/// Flash names its thinking level in the model id, and unset means the default.
fn gemini_3_8_flash(effort: Option<Effort>) -> &'static str {
    match effort {
        Some(Effort::Minimal | Effort::Low) => "gemini-3.8-flash-low",
        Some(Effort::Medium) => "gemini-3.8-flash-medium",
        Some(Effort::High | Effort::XHigh | Effort::Max) | None => "gemini-3.8-flash-high",
    }
}

fn gemini_3_7_flash(effort: Option<Effort>) -> &'static str {
    match effort {
        Some(Effort::Minimal | Effort::Low) => "gemini-3.7-flash-low",
        Some(Effort::Medium) => "gemini-3.7-flash-medium",
        Some(Effort::High | Effort::XHigh | Effort::Max) | None => "gemini-3.7-flash-high",
    }
}

/// Pro names only a low variant and an agent model for high; medium and an
/// unset effort ask for the plain id, which is the only other name the API
/// was told about.
fn gemini_3_1_pro(effort: Option<Effort>) -> &'static str {
    match effort {
        Some(Effort::Minimal | Effort::Low) => "gemini-3.1-pro-low",
        Some(Effort::High | Effort::XHigh | Effort::Max) => "gemini-pro-agent",
        Some(Effort::Medium) | None => "gemini-3.1-pro",
    }
}

/// One canonical message becomes one Gemini `content` (plus a `functionResponse`
/// content for each tool result it carries). `names` maps a tool-call id to the
/// function name, so a result can be labelled the way Gemini expects.
fn encode_message(
    message: &ModelMessage,
    names: &std::collections::HashMap<&str, &str>,
) -> Vec<Value> {
    let role = match message.role {
        ModelRole::User => "user",
        ModelRole::Assistant => "model",
    };
    let mut parts: Vec<Value> = Vec::new();
    let mut responses: Vec<Value> = Vec::new();
    for content in &message.content {
        match content {
            ModelContent::Text { text } if !text.is_empty() => parts.push(json!({"text": text})),
            ModelContent::Text { .. } => {}
            ModelContent::Image { media_type, data } => {
                parts.push(json!({"inlineData": {"mimeType": media_type, "data": data}}));
            }
            ModelContent::ToolCall {
                id,
                name,
                arguments,
            } => parts.push(json!({
                "functionCall": {"name": wire_tool_name(name), "args": arguments, "id": id},
                "thoughtSignature": "skip_thought_signature_validator",
            })),
            ModelContent::ToolResult {
                id,
                content,
                is_error,
            } => responses.push(json!({
                "functionResponse": {
                    "name": wire_tool_name(names.get(id.as_str()).copied().unwrap_or(id.as_str())),
                    "id": id,
                    "response": {
                        "output": content,
                        "error": *is_error,
                    },
                },
            })),
        }
    }
    let mut out = Vec::new();
    if !parts.is_empty() {
        out.push(json!({"role": role, "parts": parts}));
    }
    if !responses.is_empty() {
        // A tool result is a `user` turn in this dialect.
        out.push(json!({"role": "user", "parts": responses}));
    }
    out
}

fn encode_tool(tool: &ToolSchema) -> Value {
    json!({
        "name": wire_tool_name(&tool.name),
        "description": tool.description,
        "parameters": strip_unsupported_schema(&tool.input_schema),
    })
}

/// Drop the JSON Schema keywords the API rejects (`$schema`, `$ref`, `$defs`,
/// `default`, `examples`, `exclusiveMinimum`, `exclusiveMaximum`, `propertyNames`,
/// `patternProperties`, `const` at any depth) and normalize union array types
/// (`type: ["string", "null"]`) to scalar `type` plus `nullable: true`, so schemas
/// produced by standard tools (e.g. MCP servers) pass Protobuf validation.
fn strip_unsupported_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            let mut is_nullable = false;
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "$schema"
                        | "$id"
                        | "$ref"
                        | "$defs"
                        | "$comment"
                        | "definitions"
                        | "default"
                        | "examples"
                        | "exclusiveMinimum"
                        | "exclusiveMaximum"
                        | "propertyNames"
                        | "patternProperties"
                ) {
                    continue;
                }
                if key == "const" {
                    // `const: x` has no equivalent keyword here; `enum: [x]` is
                    // the documented replacement.
                    out.insert("enum".to_owned(), json!([value.clone()]));
                    continue;
                }
                if key == "type" {
                    match value {
                        Value::Array(types) => {
                            if types.iter().any(|t| t.as_str() == Some("null")) {
                                is_nullable = true;
                            }
                            let non_null: Vec<_> = types
                                .iter()
                                .filter(|t| t.as_str() != Some("null"))
                                .collect();
                            if let Some(first) = non_null.first() {
                                out.insert("type".to_owned(), (*first).clone());
                            } else {
                                out.insert("type".to_owned(), json!("string"));
                            }
                            continue;
                        }
                        Value::String(s) if s == "null" => {
                            is_nullable = true;
                            out.insert("type".to_owned(), json!("string"));
                            continue;
                        }
                        _ => {}
                    }
                }
                out.insert(key.clone(), strip_unsupported_schema(value));
            }
            if is_nullable && !out.contains_key("nullable") {
                out.insert("nullable".to_owned(), json!(true));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_unsupported_schema).collect()),
        other => other.clone(),
    }
}

impl<T: WireTransport> ModelProvider for GoogleCodeAssistProvider<T> {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn stream(&self, request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError> {
        let project = self.project()?;
        let mut wire = self.encode(request, &project);
        wire.body = self
            .redactor
            .sanitize(&wire.body)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let response = self.transport.send(wire)?;
        if response.status != 200 {
            return Err(normalize_status(response));
        }
        let tool_names: std::collections::HashMap<String, String> = request
            .tools
            .iter()
            .filter_map(|tool| {
                let mapped = wire_tool_name(&tool.name);
                (mapped != tool.name).then(|| (mapped, tool.name.clone()))
            })
            .collect();
        Ok(Box::new(EventDecoder::new(response.lines, tool_names)))
    }
}

/// The project `:loadCodeAssist` names, as a bare id or an object.
fn project_id(status: &Value) -> Option<String> {
    let project = status.get("cloudaicompanionProject")?;
    project
        .as_str()
        .or_else(|| project.get("id").and_then(Value::as_str))
        .filter(|project| !project.is_empty())
        .map(str::to_owned)
}

/// Why Google will not provision the free tier for this account, with the
/// page that fixes it, when it says so.
fn free_tier_refusal(status: &Value) -> Option<String> {
    let tier =
        |item: &Value, key: &str| item.get(key).and_then(Value::as_str) == Some(FREE_TIER_ID);
    let allowed = status
        .get("allowedTiers")
        .and_then(Value::as_array)
        .is_some_and(|tiers| tiers.iter().any(|item| tier(item, "id")));
    if allowed {
        return None;
    }
    let refused = status
        .get("ineligibleTiers")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| tier(item, "tierId"))?;
    let reason = refused.get("reasonMessage").and_then(Value::as_str)?;
    Some(match refused.get("validationUrl").and_then(Value::as_str) {
        Some(url) if !url.is_empty() => format!("{reason}\n{url}"),
        _ => reason.to_owned(),
    })
}

fn normalize_status(response: WireResponse) -> ProviderError {
    let status = response.status;
    let retry_after = response.retry_after();
    let body: String = response.lines.filter_map(Result::ok).collect();
    let value: Option<Value> = serde_json::from_str(&body).ok();
    let error = value.as_ref().and_then(|value| value.get("error"));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("http {status}"));
    let retry_after = retry_after.or_else(|| error.and_then(retry_delay));
    match status {
        401 | 403 => ProviderError::Auth(message),
        400 | 413 | 422 => ProviderError::InvalidRequest(message),
        404 => ProviderError::NotFound(message),
        429 => ProviderError::RateLimited { retry_after },
        status => ProviderError::Server { status, message },
    }
}

/// `error.details[].retryDelay` ("3.9s") to a duration.
fn retry_delay(error: &Value) -> Option<Duration> {
    let raw = error
        .get("details")?
        .as_array()?
        .iter()
        .find_map(|detail| detail.get("retryDelay").and_then(Value::as_str))?;
    let seconds: f64 = raw.trim_end_matches('s').parse().ok()?;
    Some(Duration::from_secs_f64(seconds.max(0.0)))
}

/// Decoder for a Code Assist SSE stream. Each `data:` line is one
/// `GenerateContentResponse` (possibly wrapped in `{"response": …}`).
struct EventDecoder {
    lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
    queue: VecDeque<ModelEvent>,
    /// Next tool-call index to hand out. Gemini does not number them.
    next_call: usize,
    stop: Option<StopReason>,
    done: bool,
    /// Maps sanitized wire tool names back to their canonical names when they
    /// were transformed before sending (e.g. `fs.read` → `fs_read`).
    tool_names: std::collections::HashMap<String, String>,
}

impl EventDecoder {
    fn new(
        lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
        tool_names: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            lines,
            tool_names,
            queue: VecDeque::new(),
            next_call: 0,
            stop: None,
            done: false,
        }
    }

    fn decode(&mut self, payload: &str) -> Result<(), ProviderError> {
        let outer: Value = serde_json::from_str(payload)
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        if let Some(error) = outer.get("error").filter(|error| !error.is_null()) {
            return Err(normalize_stream_error(error));
        }
        let response = outer.get("response").unwrap_or(&outer);

        if let Some(usage) = response
            .get("usageMetadata")
            .filter(|usage| !usage.is_null())
        {
            self.queue.push_back(ModelEvent::Usage {
                input_tokens: count(usage, "promptTokenCount"),
                output_tokens: count(usage, "candidatesTokenCount"),
            });
        }

        let Some(candidate) = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return Ok(());
        };

        for part in candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.decode_part(part)?;
        }

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.stop = Some(finish_reason(reason, self.next_call > 0));
        }
        Ok(())
    }

    fn decode_part(&mut self, part: &Value) -> Result<(), ProviderError> {
        if let Some(call) = part.get("functionCall") {
            let index = self.next_call;
            self.next_call += 1;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{index}"));
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() {
                return Err(ProviderError::Decode(
                    "a functionCall part named no function".to_owned(),
                ));
            }
            let canonical = self
                .tool_names
                .get(&name)
                .map(String::as_str)
                .unwrap_or(&name)
                .to_owned();
            let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
            self.queue.push_back(ModelEvent::ToolCallStarted {
                index,
                id: id.clone(),
                name: canonical.clone(),
            });
            self.queue.push_back(ModelEvent::ToolCallCompleted {
                index,
                id,
                name: canonical,
                arguments,
            });
            return Ok(());
        }
        let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return Ok(());
        };
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            self.queue.push_back(ModelEvent::ThinkingDelta {
                text: text.to_owned(),
            });
        } else {
            self.queue.push_back(ModelEvent::TextDelta {
                text: text.to_owned(),
            });
        }
        Ok(())
    }
}

impl Iterator for EventDecoder {
    type Item = Result<ModelEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            if self.done {
                return None;
            }
            let Some(line) = self.lines.next() else {
                self.done = true;
                return self
                    .stop
                    .take()
                    .map(|stop| Ok(ModelEvent::Completed { stop }));
            };
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    self.done = true;
                    return Some(Err(ProviderError::Transport(error)));
                }
            };
            let Some(payload) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            if let Err(error) = self.decode(payload) {
                self.done = true;
                self.queue.clear();
                return Some(Err(error));
            }
        }
    }
}

fn normalize_stream_error(error: &Value) -> ProviderError {
    let code = error.get("code").and_then(Value::as_u64).unwrap_or(500);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("stream error")
        .to_owned();
    match code {
        401 | 403 => ProviderError::Auth(message),
        400 => ProviderError::InvalidRequest(message),
        404 => ProviderError::NotFound(message),
        429 => ProviderError::RateLimited {
            retry_after: retry_delay(error),
        },
        code => ProviderError::Server {
            status: code as u16,
            message,
        },
    }
}

fn finish_reason(raw: &str, saw_tool_call: bool) -> StopReason {
    match raw {
        "STOP" if saw_tool_call => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => StopReason::Refusal,
        "OTHER" if saw_tool_call => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

fn count(usage: &Value, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::IdempotencyKey;
    use crate::provider::{Effort, ModelKey};
    use std::sync::Mutex as StdMutex;

    /// Answers each `send` with the next canned `(status, body)`.
    struct Canned(StdMutex<VecDeque<(u16, String)>>);

    impl WireTransport for Canned {
        fn send(&self, _request: WireRequest) -> Result<WireResponse, ProviderError> {
            let (status, body) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("a canned response");
            let lines: Vec<Result<String, String>> =
                body.lines().map(|line| Ok(line.to_owned())).collect();
            Ok(WireResponse {
                status,
                headers: vec![],
                lines: Box::new(lines.into_iter()),
            })
        }
    }

    fn canned(pairs: &[(u16, &str)]) -> Canned {
        Canned(StdMutex::new(
            pairs
                .iter()
                .map(|(status, body)| (*status, (*body).to_owned()))
                .collect(),
        ))
    }

    struct Capturing {
        response: (u16, String),
        sender: std::sync::mpsc::Sender<WireRequest>,
    }

    impl WireTransport for Capturing {
        fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
            self.sender
                .send(request)
                .expect("test receiver is available");
            let (status, body) = &self.response;
            Ok(WireResponse {
                status: *status,
                headers: vec![],
                lines: Box::new(
                    body.lines()
                        .map(|line| Ok(line.to_owned()))
                        .collect::<Vec<_>>()
                        .into_iter(),
                ),
            })
        }
    }

    fn assist(transport: Canned) -> GoogleCodeAssistProvider<Canned> {
        GoogleCodeAssistProvider::with_base_url("https://host.test", ApiKey::new("t"), transport)
            .with_onboard_poll(Duration::ZERO)
    }

    #[test]
    fn an_unprovisioned_account_is_onboarded_and_waited_for() {
        // A 200 with no tier is how Google answers an account it has not
        // provisioned; the project only exists once onboarding finishes.
        let provider = assist(canned(&[
            (200, r#"{"allowedTiers":[{"id":"free-tier"}]}"#),
            (200, r#"{"name":"op/1","done":false}"#),
            (200, r#"{"name":"op/1","done":true,"response":{}}"#),
            (
                200,
                r#"{"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-7"}"#,
            ),
        ]));
        assert_eq!(provider.discover_project().unwrap(), "proj-7");
    }

    #[test]
    fn a_provisioned_account_is_not_onboarded_again() {
        let provider = assist(canned(&[(
            200,
            r#"{"currentTier":{"id":"standard-tier"},"cloudaicompanionProject":{"id":"proj-9"}}"#,
        )]));
        assert_eq!(provider.discover_project().unwrap(), "proj-9");
    }

    #[test]
    fn a_refused_free_tier_says_why_and_where_to_fix_it() {
        let provider = assist(canned(&[(
            200,
            r#"{"ineligibleTiers":[{"tierId":"free-tier","reasonMessage":"Verify your account","validationUrl":"https://accounts.test/verify"}]}"#,
        )]));
        // Not an auth failure: signing in again cannot verify an account.
        let ProviderError::InvalidRequest(message) = provider.discover_project().unwrap_err()
        else {
            panic!("a refusal is reported as a rejected request");
        };
        assert!(message.contains("Verify your account"));
        assert!(message.contains("https://accounts.test/verify"));
    }

    #[test]
    fn a_failed_discovery_fails_the_turn_and_is_asked_again_next_time() {
        let provider = assist(canned(&[
            (200, r#"{"currentTier":{"id":"free-tier"}}"#),
            (
                200,
                r#"{"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-3"}"#,
            ),
        ]));
        assert!(provider.project().is_err());
        assert_eq!(provider.project().unwrap(), "proj-3");
    }

    #[test]
    fn available_model_discovery_identifies_as_antigravity() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let transport = Capturing {
            response: (200, r#"{"models":{"gemini-3-pro":{}}}"#.to_owned()),
            sender,
        };
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            transport,
        );

        assert_eq!(
            provider.fetch_available_models(),
            Some(vec!["gemini-3-pro".to_owned()])
        );
        let request = receiver.recv().expect("discovery request sent");
        assert_eq!(
            request.url,
            "https://host.test/v1internal:fetchAvailableModels"
        );
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "user-agent" && *value == antigravity_user_agent()));
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            model: ModelKey {
                provider: "antigravity".to_owned(),
                model: "gemini-3-pro".to_owned(),
            },
            system: Some("be brief".to_owned()),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: vec![],
            max_output_tokens: 1000,
            effort: Some(Effort::High),
            idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
        }
    }

    #[test]
    fn the_body_is_code_assist_wrapped_gemini() {
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[]),
        )
        .with_project("proj-1");
        let wire = provider.encode(&request(), "proj-1");
        assert_eq!(
            wire.url,
            "https://host.test/v1internal:streamGenerateContent?alt=sse"
        );
        let body: Value = serde_json::from_str(&wire.body).unwrap();
        assert_eq!(body["model"], json!("gemini-3-pro"));
        assert_eq!(body["project"], json!("proj-1"));
        assert_eq!(body["request"]["contents"][0]["role"], json!("user"));
        assert_eq!(
            body["request"]["contents"][0]["parts"][0]["text"],
            json!("hi")
        );
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][0]["text"],
            json!("be brief")
        );
        // High effort: budget below the output cap.
        let budget = body["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"]
            .as_u64()
            .unwrap();
        assert!(budget > 0 && budget < 1000);
        assert_eq!(body["project"], json!("proj-1"));
    }
    #[test]
    fn discovery_runs_once_then_streams() {
        let load = r#"{"cloudaicompanionProject":"discovered-proj"}"#;
        let turn = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"hmm\"},{\"text\":\"Hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1}}}\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]},\"finishReason\":\"STOP\"}]}}\n",
        );
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[(200, load), (200, turn)]),
        );
        let events: Vec<ModelEvent> = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(provider.project().unwrap(), "discovered-proj");
        assert!(matches!(
            &events[0],
            ModelEvent::Usage {
                input_tokens: 3,
                ..
            }
        ));
        assert!(matches!(&events[1], ModelEvent::ThinkingDelta { text } if text == "hmm"));
        assert!(matches!(&events[2], ModelEvent::TextDelta { text } if text == "Hi"));
        assert!(matches!(
            events.last().unwrap(),
            ModelEvent::Completed {
                stop: StopReason::EndTurn
            }
        ));
    }

    #[test]
    fn a_tool_result_is_labelled_with_the_name_of_the_call_it_answers() {
        let mut req = request();
        req.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                id: "call-7".to_owned(),
                name: "read_file".to_owned(),
                arguments: json!({"path": "x"}),
            }],
        });
        req.messages.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: "call-7".to_owned(),
                content: "contents".to_owned(),
                is_error: false,
            }],
        });
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[]),
        )
        .with_project("p");
        let body: Value = serde_json::from_str(&provider.encode(&req, "p").body).unwrap();
        let contents = body["request"]["contents"].as_array().unwrap();
        let response = contents
            .iter()
            .find_map(|content| {
                content["parts"]
                    .as_array()?
                    .iter()
                    .find_map(|part| part.get("functionResponse"))
            })
            .expect("a functionResponse part");
        assert_eq!(response["name"], json!("read_file"));
        assert_eq!(response["id"], json!("call-7"));

        assert_eq!(response["name"], json!("read_file"));
        assert_eq!(response["id"], json!("call-7"));
    }

    #[test]
    fn a_dotted_tool_result_is_labelled_with_the_sanitized_name() {
        let mut req = request();
        req.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                id: "call-8".to_owned(),
                name: "fs.read".to_owned(),
                arguments: json!({"path": "x"}),
            }],
        });
        req.messages.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: "call-8".to_owned(),
                content: "contents".to_owned(),
                is_error: false,
            }],
        });
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[]),
        )
        .with_project("p");
        let body: Value = serde_json::from_str(&provider.encode(&req, "p").body).unwrap();
        let contents = body["request"]["contents"].as_array().unwrap();
        let response = contents
            .iter()
            .find_map(|content| {
                content["parts"]
                    .as_array()?
                    .iter()
                    .find_map(|part| part.get("functionResponse"))
            })
            .expect("a functionResponse part");
        assert_eq!(response["name"], json!("fs_read"));
        assert_eq!(response["id"], json!("call-8"));
    }

    #[test]
    fn a_function_call_part_is_started_and_completed_at_once() {
        let turn = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"ls\",\"args\":{\"path\":\".\"},\"id\":\"t1\"}}]},\"finishReason\":\"OTHER\"}]}}\n";
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[(200, turn)]),
        )
        .with_project("p");
        let events: Vec<ModelEvent> = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(matches!(&events[0], ModelEvent::ToolCallStarted { name, .. } if name == "ls"));
        assert!(matches!(
            &events[1],
            ModelEvent::ToolCallCompleted { name, arguments, .. }
                if name == "ls" && arguments["path"] == json!(".")
        ));
        assert!(matches!(
            events.last().unwrap(),
            ModelEvent::Completed {
                stop: StopReason::ToolUse
            }
        ));
    }

    #[test]
    fn a_failed_discovery_fails_the_turn_instead_of_sending_no_project() {
        // Sending no project gets a bare 403 from Code Assist; the reason the
        // discovery failed is the one worth showing.
        let turn = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n";
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[
                (500, "nope"),
                (200, r#"{"cloudaicompanionProject":"proj-2"}"#),
                (200, turn),
            ]),
        );
        assert!(matches!(
            provider.stream(&request()),
            Err(ProviderError::Server { status: 500, .. })
        ));
        let events: Vec<ModelEvent> = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(provider.project().unwrap(), "proj-2");
        assert!(events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta { text } if text == "ok")));
    }

    #[test]
    fn an_error_body_is_normalized() {
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[(429, r#"{"error":{"code":429,"message":"slow","status":"RESOURCE_EXHAUSTED","details":[{"retryDelay":"2s"}]}}"#)]),
        )
        .with_project("p");
        let error = provider
            .stream(&request())
            .err()
            .expect("a 429 is an error");
        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after: Some(delay),
            } if delay == Duration::from_secs(2)
        ));
    }

    #[test]
    fn unsupported_schema_keywords_are_stripped() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "kind": {"const": "email"},
                "note": {"type": "string", "default": "none"},
                "limit": {"type": ["integer", "null"], "exclusiveMinimum": 0},
                "metadata": {
                    "type": "object",
                    "propertyNames": {"pattern": "^[a-z]+$"},
                    "patternProperties": {
                        "^[a-z]+$": {"type": "string"}
                    }
                }
            },
        });
        let out = strip_unsupported_schema(&schema);
        assert!(out.get("$schema").is_none());
        assert_eq!(out["properties"]["kind"]["enum"], json!(["email"]));
        assert!(out["properties"]["note"].get("default").is_none());
        assert_eq!(out["properties"]["limit"]["type"], json!("integer"));
        assert_eq!(out["properties"]["limit"]["nullable"], json!(true));
        assert!(out["properties"]["limit"].get("exclusiveMinimum").is_none());
        assert!(out["properties"]["metadata"].get("propertyNames").is_none());
        assert!(out["properties"]["metadata"]
            .get("patternProperties")
            .is_none());
    }

    #[test]
    fn a_dotted_tool_name_is_sanitized_and_mapped_back() {
        let mut req = request();
        req.tools.push(ToolSchema {
            name: "fs.read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: json!({"type": "object"}),
        });
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[]),
        )
        .with_project("p");
        let body: Value = serde_json::from_str(&provider.encode(&req, "p").body).unwrap();
        let declaration = body["request"]["tools"][0]["functionDeclarations"][0].clone();
        assert_eq!(
            declaration["name"],
            json!("fs_read"),
            "dotted tool names are sanitized to match the wire-name pattern"
        );

        let turn = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"fs_read\",\"args\":{\"path\":\"Cargo.toml\"},\"id\":\"call-1\"}}]},\"finishReason\":\"OTHER\"}]}}\n";
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[(200, turn)]),
        )
        .with_project("p");
        let events: Vec<ModelEvent> = provider.stream(&req).unwrap().map(Result::unwrap).collect();
        assert!(matches!(
            &events[0],
            ModelEvent::ToolCallStarted { name, .. } if name == "fs.read"
        ));
        assert!(matches!(
            &events[1],
            ModelEvent::ToolCallCompleted { name, .. } if name == "fs.read"
        ));
    }
}
