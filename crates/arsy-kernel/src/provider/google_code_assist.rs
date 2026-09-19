//! Google Cloud Code Assist adapter, as Antigravity speaks it.
//!
//! Not Chat Completions and not Anthropic Messages: a Gemini
//! `GenerateContentRequest` (`contents` of `parts`, `systemInstruction` as an
//! object, tools as `functionDeclarations`) wrapped in a Code Assist envelope
//! that names the model and a Google Cloud project. The project is discovered
//! once with `:loadCodeAssist` and cached; when the account has none of its
//! own, a shared fallback stands in.
//!
//! The response is Gemini-shaped too: SSE lines of
//! `{"response":{"candidates":[{"content":{"parts":[…]},"finishReason":…}]}}`.
//! A `functionCall` part arrives whole, so a tool call is started and completed
//! from the same part.

use super::{
    wire::{ApiKey, WireRequest, WireResponse, WireTransport},
    CanonicalModelRequest, ModelContent, ModelEvent, ModelEventStream, ModelMessage, ModelProvider,
    ModelRole, ProviderDescriptor, ProviderError, StopReason, ToolSchema,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::{collections::VecDeque, sync::Mutex, time::Duration};

pub const DEFAULT_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";
/// Antigravity User-Agent matching official antigravity/hub client.
const USER_AGENT: &str =
    "antigravity/hub/2.8.0 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)";

pub struct GoogleCodeAssistProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    transport: T,
    redactor: Redactor,
    /// Resolved lazily on the first `stream` and reused after. `None` means not
    /// looked up yet; `Some("")` is possible and means "send no project".
    project: Mutex<Option<String>>,
}

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

    /// Pin the project rather than discovering it. Mainly for tests.
    pub fn with_project(self, project: impl Into<String>) -> Self {
        *self.project.lock().unwrap() = Some(project.into());
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
            ("user-agent".to_owned(), USER_AGENT.to_owned()),
        ];
        if streaming {
            headers.push(("accept".to_owned(), "text/event-stream".to_owned()));
        }
        headers
    }

    /// The project to send, discovering it once. A discovery that fails for any
    /// reason falls back rather than failing the turn: an empty or shared
    /// project still authorizes a personal account.
    ///
    /// ponytail: `:loadCodeAssist` only, no `:onboardUser`. An account that has
    /// opened Antigravity once is already onboarded; add the onboard call if a
    /// brand-new account ever needs to work without opening the IDE first.
    fn project(&self) -> String {
        let mut cached = self.project.lock().unwrap();
        if let Some(project) = cached.as_ref() {
            return project.clone();
        }
        let discovered = self.discover_project().unwrap_or_default();
        *cached = Some(discovered.clone());
        discovered
    }

    fn discover_project(&self) -> Option<String> {
        let load_request = WireRequest {
            url: self.action_url("loadCodeAssist"),
            headers: self.headers("", false),
            body: json!({
                "metadata": {
                    "ideType": "ANTIGRAVITY",
                }
            })
            .to_string(),
        };

        if let Ok(response) = self.transport.send(load_request) {
            if response.status == 200 {
                let body: String = response.lines.filter_map(Result::ok).collect();
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    let project = value
                        .get("cloudaicompanionProject")
                        .and_then(|p| {
                            p.as_str()
                                .map(str::to_owned)
                                .or_else(|| p.get("id").and_then(Value::as_str).map(str::to_owned))
                        })
                        .filter(|p| !p.is_empty());
                    if let Some(proj) = project {
                        return Some(proj);
                    }
                }
            } else if response.status == 403 || response.status == 404 {
                // Onboard free-tier if needed
                let onboard = WireRequest {
                    url: self.action_url("onboardUser"),
                    headers: self.headers("", false),
                    body: json!({
                        "tierId": "free-tier",
                        "metadata": {
                            "ideType": "ANTIGRAVITY",
                        }
                    })
                    .to_string(),
                };
                let _ = self.transport.send(onboard);

                // Retry loadCodeAssist
                let retry_req = WireRequest {
                    url: self.action_url("loadCodeAssist"),
                    headers: self.headers("", false),
                    body: json!({
                        "metadata": {
                            "ideType": "ANTIGRAVITY",
                        }
                    })
                    .to_string(),
                };
                if let Ok(retry) = self.transport.send(retry_req) {
                    if retry.status == 200 {
                        let body: String = retry.lines.filter_map(Result::ok).collect();
                        if let Ok(value) = serde_json::from_str::<Value>(&body) {
                            let project = value.get("cloudaicompanionProject");
                            return project
                                .and_then(|p| {
                                    p.as_str().map(str::to_owned).or_else(|| {
                                        p.get("id").and_then(Value::as_str).map(str::to_owned)
                                    })
                                })
                                .filter(|p| !p.is_empty());
                        }
                    }
                }
            }
        }
        None
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

fn routed_wire_model(model: &str, effort: Option<crate::provider::Effort>) -> &str {
    use crate::provider::Effort;
    match (model, effort) {
        ("gemini-3.8-flash", Some(Effort::Low)) => "gemini-3.8-flash-low",
        ("gemini-3.8-flash", Some(Effort::Medium)) => "gemini-3.8-flash-medium",
        ("gemini-3.8-flash", Some(Effort::High) | None) => "gemini-3.8-flash-high",

        ("gemini-3.7-flash", Some(Effort::Low)) => "gemini-3.7-flash-low",
        ("gemini-3.7-flash", Some(Effort::Medium)) => "gemini-3.7-flash-medium",
        ("gemini-3.7-flash", Some(Effort::High) | None) => "gemini-3.7-flash-high",

        ("gemini-3.1-pro", Some(Effort::Low)) => "gemini-3.1-pro-low",
        ("gemini-3.1-pro", Some(Effort::Medium)) => "gemini-3.1-pro-high",
        ("gemini-3.1-pro", Some(Effort::High) | None) => "gemini-pro-agent",

        ("claude-3-7-sonnet", Some(Effort::Medium | Effort::High)) => "claude-3-7-sonnet-thinking",
        ("claude-sonnet-4-5", Some(Effort::Medium | Effort::High)) => "claude-sonnet-4-5-thinking",
        ("claude-sonnet-4-6", Some(Effort::Medium | Effort::High)) => "claude-sonnet-4-6-thinking",
        ("claude-opus-4-5", Some(Effort::Medium | Effort::High)) => "claude-opus-4-5-thinking",
        ("claude-opus-4-6", Some(Effort::Medium | Effort::High)) => "claude-opus-4-6-thinking",

        (other, _) => other,
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
                "functionCall": {"name": name, "args": arguments, "id": id},
                "thoughtSignature": "skip_thought_signature_validator",
            })),
            ModelContent::ToolResult {
                id,
                content,
                is_error,
            } => responses.push(json!({
                "functionResponse": {
                    "name": names.get(id.as_str()).copied().unwrap_or(id.as_str()),
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
        "name": tool.name,
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
        let project = self.project();
        let mut wire = self.encode(request, &project);
        wire.body = self
            .redactor
            .sanitize(&wire.body)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let response = self.transport.send(wire)?;
        if response.status != 200 {
            return Err(normalize_status(response));
        }
        Ok(Box::new(EventDecoder::new(response.lines)))
    }
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
}

impl EventDecoder {
    fn new(lines: Box<dyn Iterator<Item = Result<String, String>> + Send>) -> Self {
        Self {
            lines,
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
            let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
            self.queue.push_back(ModelEvent::ToolCallStarted {
                index,
                id: id.clone(),
                name: name.clone(),
            });
            self.queue.push_back(ModelEvent::ToolCallCompleted {
                index,
                id,
                name,
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
        assert_eq!(provider.project(), "discovered-proj");
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
    fn discovery_failure_falls_back_and_still_streams() {
        let turn = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n";
        let provider = GoogleCodeAssistProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            canned(&[(500, "nope"), (200, turn)]),
        );
        let events: Vec<ModelEvent> = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(provider.project(), "");
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
}
