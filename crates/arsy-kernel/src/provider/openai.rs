//! OpenAI-compatible Chat Completions adapter.
//!
//! Written against the dialect rather than against one vendor: the base URL is
//! supplied by configuration, so the same adapter serves the OpenAI API, a
//! gateway such as LiteLLM or OpenRouter, and a local runtime such as Ollama
//! or LM Studio. Nothing here assumes a particular host.
//!
//! HTTP is injected as [`WireTransport`], so the part that differs per
//! provider — body shape, headers, status mapping, SSE semantics — is the part
//! that is testable without a network.

use super::{
    wire::{ApiKey, WireRequest, WireResponse, WireTransport},
    CanonicalModelRequest, ModelContent, ModelEvent, ModelEventStream, ModelMessage, ModelProvider,
    ModelRole, ProviderDescriptor, ProviderError, StopReason, ToolSchema,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Sentinel that ends an OpenAI-style stream. It is not JSON, so it has to be
/// recognized before decoding.
const DONE: &str = "[DONE]";

pub struct OpenAiProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    transport: T,
    redactor: Redactor,
    sanitize_tool_names: bool,
}

impl<T: WireTransport> OpenAiProvider<T> {
    pub fn new(key: ApiKey, transport: T) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key, transport)
    }

    /// `base_url` is the API root, the same value an OpenAI SDK would take, so
    /// it already includes any version segment the host uses.
    pub fn with_base_url(base_url: impl Into<String>, key: ApiKey, transport: T) -> Self {
        Self {
            descriptor: ProviderDescriptor {
                id: "openai".to_owned(),
                max_retries: 3,
            },
            base_url: base_url.into(),
            key,
            transport,
            redactor: Redactor::new(),
            sanitize_tool_names: false,
        }
    }

    /// Same adapter under a different provider id, so a gateway appears in
    /// events and errors under the name the operator configured for it.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.descriptor.id = id.into();
        self
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Send `fs.read` as `fs_read` for a host that rejects dots in a function
    /// name, and read its calls back under the canonical name.
    pub fn with_sanitized_tool_names(mut self, enabled: bool) -> Self {
        self.sanitize_tool_names = enabled;
        self
    }

    /// Canonical request to Chat Completions wire form.
    pub fn encode(&self, request: &CanonicalModelRequest) -> WireRequest {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(request.model.model));
        body.insert("max_tokens".to_owned(), json!(request.max_output_tokens));
        body.insert("stream".to_owned(), json!(true));
        // Usage is omitted from a stream unless it is asked for. A host that
        // does not know the option ignores it.
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));

        // This dialect takes the level by name. A host that has no reasoning
        // model ignores the field, and an unset effort omits it entirely.
        if let Some(effort) = request.effort {
            body.insert("reasoning_effort".to_owned(), json!(effort.as_str()));
        }

        // The system prompt is a message in this dialect, not a field.
        let mut messages: Vec<Value> = request
            .system
            .iter()
            .map(|system| json!({"role": "system", "content": system}))
            .collect();
        for message in &request.messages {
            encode_message(message, &mut messages, self.sanitize_tool_names);
        }
        body.insert("messages".to_owned(), Value::Array(messages));

        if !request.tools.is_empty() {
            body.insert(
                "tools".to_owned(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| encode_tool(tool, self.sanitize_tool_names))
                        .collect(),
                ),
            );
            // Keep model-generated calls aligned with the host's sequential
            // execution and the TUI's one-card-at-a-time presentation.
            body.insert("parallel_tool_calls".to_owned(), json!(false));
        }
        WireRequest {
            url: format!("{}/chat/completions", self.base_url.trim_end_matches('/')),
            headers: vec![
                (
                    "authorization".to_owned(),
                    format!("Bearer {}", self.key.expose()),
                ),
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "text/event-stream".to_owned()),
            ],
            body: Value::Object(body).to_string(),
        }
    }
}

fn wire_tool_name(name: &str, sanitize: bool) -> String {
    if sanitize {
        name.replace('.', "_")
    } else {
        name.to_owned()
    }
}

fn encode_tool(tool: &ToolSchema, sanitize: bool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": wire_tool_name(&tool.name, sanitize),
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

/// One canonical message becomes one or more wire messages.
///
/// A tool result is its own `role: "tool"` message in this dialect, so a
/// canonical message that mixes text and results cannot map one-to-one.
fn encode_message(message: &ModelMessage, out: &mut Vec<Value>, sanitize: bool) {
    let role = match message.role {
        ModelRole::User => "user",
        ModelRole::Assistant => "assistant",
    };
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    // Empty unless the message carries an image. A text-only message keeps the
    // plain-string `content` every existing recording and test expects; the
    // array form is used only where it is actually needed.
    let mut images: Vec<Value> = Vec::new();
    for content in &message.content {
        match content {
            // Chat Completions has no place to send reasoning state back.
            ModelContent::Reasoning { .. } => {}
            ModelContent::Text { text: chunk } => text.push_str(chunk),
            ModelContent::Image { media_type, data } => images.push(json!({
                "type": "image_url",
                "image_url": {"url": super::data_url(media_type, data)},
            })),
            ModelContent::ToolCall {
                id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": wire_tool_name(name, sanitize),
                    "arguments": arguments.to_string(),
                },
            })),
            // Emitted below, after the message it belongs to.
            ModelContent::ToolResult { .. } => {}
        }
    }
    if !text.is_empty() || !tool_calls.is_empty() || !images.is_empty() {
        let mut wire = Map::new();
        wire.insert("role".to_owned(), json!(role));
        if images.is_empty() {
            wire.insert("content".to_owned(), json!(text));
        } else {
            wire.insert(
                "content".to_owned(),
                Value::Array(super::content_parts("text", &text, &mut images)),
            );
        }
        if !tool_calls.is_empty() {
            wire.insert("tool_calls".to_owned(), Value::Array(tool_calls));
        }
        out.push(Value::Object(wire));
    }
    for content in &message.content {
        if let ModelContent::ToolResult {
            id,
            content,
            is_error,
        } = content
        {
            // The dialect has no error flag on a tool result, so the fact is
            // carried in the content rather than dropped.
            let body = if *is_error {
                format!("error: {content}")
            } else {
                content.clone()
            };
            out.push(json!({"role": "tool", "tool_call_id": id, "content": body}));
        }
    }
}

impl<T: WireTransport> ModelProvider for OpenAiProvider<T> {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn stream(&self, request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError> {
        let mut wire = self.encode(request);
        wire.body = self
            .redactor
            .sanitize(&wire.body)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let response = self.transport.send(wire)?;
        if response.status != 200 {
            return Err(normalize_status(response));
        }
        let mut decoder = EventDecoder::new(response.lines);
        if self.sanitize_tool_names {
            decoder.canonical_names = request
                .tools
                .iter()
                .map(|tool| (wire_tool_name(&tool.name, true), tool.name.clone()))
                .collect();
        }
        Ok(Box::new(decoder))
    }
}

/// HTTP failure to normalized error, including the provider's retry hint.
///
/// A non-200 response carries a JSON error body rather than an event stream,
/// so the same line iterator is drained as the body.
fn normalize_status(response: WireResponse) -> ProviderError {
    let status = response.status;
    let retry_after = response.retry_after();
    let body: String = response.lines.filter_map(Result::ok).collect();
    let message = error_message(&body).unwrap_or_else(|| format!("http {status}"));
    match status {
        401 | 403 => ProviderError::Auth(message),
        400 | 413 | 422 => ProviderError::InvalidRequest(message),
        404 => ProviderError::NotFound(message),
        429 => ProviderError::RateLimited { retry_after },
        status => ProviderError::Server { status, message },
    }
}

fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let detail = error.get("message").and_then(Value::as_str)?;
    Some(format!("{kind}: {detail}"))
}

/// Server-sent-event decoder for a Chat Completions stream.
///
/// Tool arguments arrive as `function.arguments` fragments that are only valid
/// JSON once concatenated. Fragments are surfaced as
/// [`ModelEvent::ToolCallDelta`] and buffered; a call becomes executable only
/// when the stream reports a finish reason and the accumulated text parses. A
/// stream that is cut short therefore yields no executable call.
struct EventDecoder {
    lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
    calls: Vec<ToolCall>,
    /// One wire chunk can carry several canonical facts.
    queue: VecDeque<ModelEvent>,
    /// Held back until the stream really ends.
    ///
    /// This dialect reports usage in a trailing chunk *after* the one carrying
    /// the finish reason, so emitting `Completed` as soon as it is seen would
    /// put usage after the terminal event and hide it from a caller that stops
    /// reading there. Every adapter ends with `Completed`.
    stop: Option<StopReason>,
    done: bool,
    /// Wire name to canonical name, filled only when names were sanitized.
    canonical_names: BTreeMap<String, String>,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl EventDecoder {
    fn new(lines: Box<dyn Iterator<Item = Result<String, String>> + Send>) -> Self {
        Self {
            lines,
            calls: Vec::new(),
            queue: VecDeque::new(),
            stop: None,
            done: false,
            canonical_names: BTreeMap::new(),
        }
    }

    /// Decode one `data:` payload into zero or more canonical events.
    fn decode(&mut self, payload: &str) -> Result<(), ProviderError> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        // A gateway may report a mid-stream failure as an error object in the
        // stream rather than as a status. Some send the key on every chunk with
        // a null value, which is the absence of an error, not one.
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(normalize_stream_error(error));
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.queue.push_back(ModelEvent::Usage {
                input_tokens: count(usage, "prompt_tokens"),
                output_tokens: count(usage, "completion_tokens"),
            });
        }
        // The final usage-only chunk carries an empty `choices` array.
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.queue.push_back(ModelEvent::TextDelta {
                        text: text.to_owned(),
                    });
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_call_delta(call)?;
                }
            }
            // Reasoning models surface their thinking in a sibling field of
            // the same delta, before the answer text it produced.
            if let Some(thinking) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.queue.push_back(ModelEvent::ThinkingDelta {
                    text: thinking.to_owned(),
                });
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            // Every buffered call must parse as a whole here, or it is
            // rejected rather than guessed at.
            for index in 0..self.calls.len() {
                self.complete_tool_call(index)?;
            }
            // Some hosts repeat the finish reason on a later chunk; the calls
            // already emitted must not be emitted a second time.
            self.calls.clear();
            self.stop = Some(stop_reason(finish));
        }
        Ok(())
    }

    /// Accumulate one fragment. The wire `index` orders calls within the
    /// message and is the only stable way to tell them apart, because a
    /// fragment after the first carries neither id nor name.
    ///
    /// Some compatible hosts leave `index` out. Such a fragment belongs to the
    /// call its `id` names, or opens a new call when the id is new, or else
    /// continues the latest call.
    fn tool_call_delta(&mut self, call: &Value) -> Result<(), ProviderError> {
        /// More calls than any message carries; an index past it is a broken
        /// stream, not a reason to allocate that many slots.
        const MAX_CALLS: usize = 256;

        let id = call.get("id").and_then(Value::as_str);
        let index = match call.get("index").and_then(Value::as_u64) {
            Some(index) => usize::try_from(index).unwrap_or(usize::MAX),
            None => match id {
                Some(id) => self
                    .calls
                    .iter()
                    .position(|known| known.id == id)
                    .unwrap_or(self.calls.len()),
                None => self.calls.len().saturating_sub(1),
            },
        };
        if index >= MAX_CALLS {
            return Err(ProviderError::Decode(format!(
                "tool call index {index} is out of range"
            )));
        }
        if index >= self.calls.len() {
            // Tolerate a gap rather than fail: an absent slot decodes as an
            // empty call and is rejected later if it never completes.
            self.calls.resize_with(index + 1, ToolCall::default);
        }
        let function = call.get("function");
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str);
        let started = self.calls[index].id.is_empty() && self.calls[index].name.is_empty();
        if let Some(id) = id {
            self.calls[index].id = id.to_owned();
        }
        if let Some(name) = name {
            self.calls[index].name = self
                .canonical_names
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_owned());
        }
        if started && (id.is_some() || name.is_some()) {
            self.queue.push_back(ModelEvent::ToolCallStarted {
                index,
                id: self.calls[index].id.clone(),
                name: self.calls[index].name.clone(),
            });
        }
        if let Some(fragment) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .filter(|fragment| !fragment.is_empty())
        {
            self.calls[index].arguments.push_str(fragment);
            self.queue.push_back(ModelEvent::ToolCallDelta {
                index,
                fragment: fragment.to_owned(),
            });
        }
        Ok(())
    }

    /// The only place a tool call becomes executable.
    fn complete_tool_call(&mut self, index: usize) -> Result<(), ProviderError> {
        let call = &self.calls[index];
        if call.name.is_empty() {
            return Err(ProviderError::Decode(format!(
                "tool call {index} never named a function"
            )));
        }
        // An empty-input tool call is legal and encodes as `{}`.
        let raw = if call.arguments.trim().is_empty() {
            "{}"
        } else {
            &call.arguments
        };
        let arguments = serde_json::from_str(raw).map_err(|error| {
            ProviderError::Decode(format!(
                "tool arguments for call {index} never completed: {error}"
            ))
        })?;
        self.queue.push_back(ModelEvent::ToolCallCompleted {
            index,
            id: call.id.clone(),
            name: call.name.clone(),
            arguments,
        });
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
                // A stream that ended without a finish reason was cut short;
                // it terminates without a `Completed` rather than inventing one.
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
            if payload == DONE {
                self.done = true;
                return self
                    .stop
                    .take()
                    .map(|stop| Ok(ModelEvent::Completed { stop }));
            }
            if let Err(error) = self.decode(payload) {
                self.done = true;
                self.queue.clear();
                return Some(Err(error));
            }
        }
    }
}

/// Classify an error an OpenAI-family stream reported mid-response, for both
/// Chat Completions and Responses.
///
/// The class may be named by `code` (a string, or an HTTP status as a
/// number), by `type`, or by both — `response.failed` and many proxies send
/// only `code`, while OpenAI's own errors pair a broad `type` with a precise
/// `code`. The first of them that names a known class decides, precise one
/// first. That matters most for a request that is itself at fault — too long
/// for the context window, a prompt the model refuses, an image it cannot
/// read: it fails identically every time, so it must not be retried as a
/// server fault nor kept in history as one.
pub(super) fn normalize_stream_error(error: &Value) -> ProviderError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("stream error")
        .to_owned();
    let class = |kind: &str| match kind {
        "authentication_error" | "permission_error" | "invalid_api_key" => {
            Some(ProviderError::Auth(message.clone()))
        }
        "invalid_request_error" | "invalid_prompt" | "context_length_exceeded" => {
            Some(ProviderError::InvalidRequest(message.clone()))
        }
        kind if kind.contains("image") => Some(ProviderError::InvalidRequest(message.clone())),
        "not_found_error" => Some(ProviderError::NotFound(message.clone())),
        "rate_limit_error" | "rate_limit_exceeded" => {
            Some(ProviderError::RateLimited { retry_after: None })
        }
        _ => None,
    };
    let status = match error.get("code").and_then(Value::as_u64) {
        Some(400 | 413 | 422) => Some("invalid_request_error"),
        Some(401 | 403) => Some("authentication_error"),
        Some(404) => Some("not_found_error"),
        Some(429) => Some("rate_limit_error"),
        _ => None,
    };
    [
        error.get("code").and_then(Value::as_str),
        error.get("type").and_then(Value::as_str),
        status,
    ]
    .into_iter()
    .flatten()
    .find_map(class)
    .unwrap_or(ProviderError::Server {
        status: 500,
        message,
    })
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

fn count(usage: &Value, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_gateway_error_named_only_by_code_is_classified_by_it() {
        // Many proxies send no `type`, only `code`: a class name or a status.
        for error in [
            json!({"code": "context_length_exceeded", "message": "too long"}),
            json!({"code": 400, "message": "prompt too long"}),
            json!({"code": 413, "message": "payload too large"}),
        ] {
            assert!(
                matches!(
                    normalize_stream_error(&error),
                    ProviderError::InvalidRequest(_)
                ),
                "{error}"
            );
        }
        // A broad `type` with a precise `code`: the precise one decides, and
        // a `code` nobody knows defers to the `type` instead of to a guess.
        assert!(matches!(
            normalize_stream_error(&json!({
                "type": "invalid_request_error", "code": "invalid_api_key", "message": "bad key"
            })),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            normalize_stream_error(&json!({
                "type": "invalid_request_error", "code": "unsupported_parameter", "message": "no"
            })),
            ProviderError::InvalidRequest(_)
        ));
        assert!(matches!(
            normalize_stream_error(&json!({"code": 429, "message": "slow down"})),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            normalize_stream_error(&json!({"code": 502, "message": "upstream"})),
            ProviderError::Server { .. }
        ));
    }
}
