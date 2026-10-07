//! OpenAI Responses API adapter, as the Codex / ChatGPT backend speaks it.
//!
//! Chat Completions and Responses share a vendor but not a wire: the request
//! carries `instructions` and an `input` array of typed items rather than
//! `messages`, and the stream is a sequence of named events
//! (`response.output_text.delta`, `response.completed`, …) rather than choice
//! deltas. The ChatGPT-plan backend also insists on `store: false` and a
//! non-empty `instructions`, and scopes the call to one account through a
//! `chatgpt-account-id` header read from the access token's own claims.
//!
//! HTTP is injected as [`WireTransport`], so body shape, headers, and SSE
//! semantics stay testable without a network.

use super::{
    wire::{ApiKey, WireRequest, WireResponse, WireTransport},
    CanonicalModelRequest, ModelContent, ModelEvent, ModelEventStream, ModelMessage, ModelProvider,
    ModelRole, ProviderDescriptor, ProviderError, StopReason, ToolSchema,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Sent so the backend attributes the call to a Codex-style client, which is
/// what the ChatGPT-plan entitlement is granted to.
const ORIGINATOR: &str = "codex_cli_rs";

/// The backend rejects a request with no system prompt; this stands in when the
/// canonical request carries none.
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful coding assistant.";

pub struct OpenAiResponsesProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    account_id: Option<String>,
    transport: T,
    redactor: Redactor,
}

impl<T: WireTransport> OpenAiResponsesProvider<T> {
    pub fn new(key: ApiKey, transport: T) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key, transport)
    }

    pub fn with_base_url(base_url: impl Into<String>, key: ApiKey, transport: T) -> Self {
        // The ChatGPT-plan access token is itself a JWT naming the account the
        // entitlement belongs to. Reading it here means the caller does not
        // have to thread the account id through credential resolution.
        let account_id = crate::oauth::jwt_claim(key.expose(), "chatgpt_account_id");
        Self {
            descriptor: ProviderDescriptor {
                id: "openai_responses".to_owned(),
                max_retries: 3,
            },
            base_url: base_url.into(),
            key,
            account_id,
            transport,
            redactor: Redactor::new(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.descriptor.id = id.into();
        self
    }

    /// Override the account the call is billed to. Only needed when the token
    /// is not a JWT, or names more than one account.
    pub fn with_account_id(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn encode(&self, request: &CanonicalModelRequest) -> WireRequest {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(request.model.model));
        body.insert(
            "instructions".to_owned(),
            json!(request
                .system
                .as_deref()
                .filter(|system| !system.trim().is_empty())
                .unwrap_or(DEFAULT_INSTRUCTIONS)),
        );
        // The ChatGPT Codex backend rejects caller-supplied output caps.
        // Generic Responses endpoints accept max_output_tokens normally.
        if !matches!(self.descriptor.id.as_str(), "codex" | "codex-oauth") {
            body.insert(
                "max_output_tokens".to_owned(),
                json!(request.max_output_tokens),
            );
        }
        body.insert("stream".to_owned(), json!(true));
        // The backend requires an explicit `false`: it is stateless, so every
        // turn already carries its whole history in `input`.
        body.insert("store".to_owned(), json!(false));
        // Stateless, so the model's reasoning survives a tool call only if it
        // comes back encrypted and is sent again with the history. A reasoning
        // model on the Codex backend reasons whether or not an effort is set.
        let codex = matches!(self.descriptor.id.as_str(), "codex" | "codex-oauth");
        let include = if request.effort.is_some() || codex {
            json!(["reasoning.encrypted_content"])
        } else {
            json!([])
        };
        body.insert("include".to_owned(), include);

        if let Some(effort) = request.effort {
            body.insert(
                "reasoning".to_owned(),
                json!({"effort": effort.as_str(), "summary": "auto"}),
            );
        }

        let mut input: Vec<Value> = Vec::new();
        for message in &request.messages {
            encode_message(
                message,
                &mut input,
                &self.descriptor.id,
                &request.model.model,
            );
        }
        body.insert("input".to_owned(), Value::Array(input));

        if !request.tools.is_empty() {
            body.insert(
                "tools".to_owned(),
                Value::Array(request.tools.iter().map(encode_tool).collect()),
            );
            body.insert("tool_choice".to_owned(), json!("auto"));
            // TUI executes one tool card at a time; keeping the provider on
            // that same cadence avoids a burst of calls that looks concurrent.
            body.insert("parallel_tool_calls".to_owned(), json!(false));
        }

        let mut headers = vec![
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.key.expose()),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("originator".to_owned(), ORIGINATOR.to_owned()),
        ];
        if let Some(account) = &self.account_id {
            headers.push(("chatgpt-account-id".to_owned(), account.clone()));
        }

        WireRequest {
            url: format!("{}/responses", self.base_url.trim_end_matches('/')),
            headers,
            body: Value::Object(body).to_string(),
        }
    }
}
fn encode_tool(tool: &ToolSchema) -> Value {
    // Codex's Responses backend only accepts alphanumeric, `_`, and `-` in
    // function names. ARSY's canonical names use dots (`fs.read`, `fs.edit`).
    json!({
        "type": "function",
        "name": codex_tool_name(&tool.name),
        "description": tool.description,
        "parameters": tool.input_schema,
    })
}

fn codex_tool_name(name: &str) -> String {
    name.replace('.', "_")
}

/// One canonical message becomes one or more Responses `input` items.
fn encode_message(message: &ModelMessage, out: &mut Vec<Value>, adapter: &str, model: &str) {
    let (role, text_type) = match message.role {
        ModelRole::User => ("user", "input_text"),
        ModelRole::Assistant => ("assistant", "output_text"),
    };
    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    for content in &message.content {
        match content {
            // Ahead of the round's calls, where the model produced it.
            ModelContent::Reasoning { state } => {
                if let Some(item) = super::reasoning_payload(state, adapter, model) {
                    out.push(item.clone());
                }
            }
            ModelContent::Text { text: chunk } => text.push_str(chunk),
            ModelContent::Image { media_type, data } => images.push(json!({
                "type": "input_image",
                "image_url": super::data_url(media_type, data),
            })),
            ModelContent::ToolCall {
                id,
                name,
                arguments,
            } => out.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": codex_tool_name(name),
                "arguments": arguments.to_string(),
            })),
            ModelContent::ToolResult { .. } => {}
        }
    }
    // Tool outputs first: each answers the `function_call` just before it,
    // and the API expects nothing between a call and its output. Text in the
    // same message — an operator steering the turn mid-way — follows them.
    for content in &message.content {
        if let ModelContent::ToolResult {
            id,
            content,
            is_error,
        } = content
        {
            let output = if *is_error {
                format!("error: {content}")
            } else {
                content.clone()
            };
            out.push(json!({
                "type": "function_call_output",
                "call_id": id,
                "output": output,
            }));
        }
    }
    if !text.is_empty() || !images.is_empty() {
        let parts = super::content_parts(text_type, &text, &mut images);
        out.push(json!({"type": "message", "role": role, "content": parts}));
    }
}

impl<T: WireTransport> ModelProvider for OpenAiResponsesProvider<T> {
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
        let tool_names = request
            .tools
            .iter()
            .map(|tool| (codex_tool_name(&tool.name), tool.name.clone()))
            .filter(|(wire, canonical)| wire != canonical)
            .collect();
        Ok(Box::new(EventDecoder::new(
            response.lines,
            &self.descriptor.id,
            &request.model.model,
            tool_names,
        )))
    }
}

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
    let error = value.get("error").or_else(|| value.get("detail"))?;
    if let Some(text) = error.as_str() {
        return Some(text.to_owned());
    }
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let detail = error.get("message").and_then(Value::as_str)?;
    Some(format!("{kind}: {detail}"))
}

/// Decoder for a Responses event stream.
///
/// Each `data:` line is a JSON object with a `type`. Tool-call arguments arrive
/// as `response.function_call_arguments.delta` fragments and are only valid
/// JSON once the matching `response.output_item.done` reports the whole string,
/// so a call becomes executable only there. A stream cut short yields no
/// executable call.
struct EventDecoder {
    lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
    /// `output_index` -> the call being assembled at it.
    calls: Vec<Option<ToolCall>>,
    queue: VecDeque<ModelEvent>,
    /// Set once a `function_call` item is seen, so the terminal event reports
    /// `ToolUse` even if the backend's own status says `completed`.
    saw_tool_call: bool,
    stop: Option<StopReason>,
    done: bool,
    /// Who tags the reasoning items this stream returns.
    adapter: String,
    model: String,
    /// Wire names back to canonical ones, for every tool the request offered
    /// under a name the wire would not take (`fs.read` goes as `fs_read`).
    /// Built from the request, so a tool added later — `mcp.load`, a plugin's
    /// — maps back without anyone remembering to list it.
    tool_names: std::collections::HashMap<String, String>,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl EventDecoder {
    fn new(
        lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
        adapter: &str,
        model: &str,
        tool_names: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            lines,
            calls: Vec::new(),
            queue: VecDeque::new(),
            saw_tool_call: false,
            stop: None,
            done: false,
            adapter: adapter.to_owned(),
            model: model.to_owned(),
            tool_names,
        }
    }

    /// An output item is finished: keep it if it is reasoning, and complete
    /// the call assembled at `index` if it is one.
    fn finish_item(&mut self, index: usize, item: Option<&Value>) -> Result<(), ProviderError> {
        if let Some(item) = item {
            self.keep_reasoning(item);
        }
        let whole = item
            .and_then(|item| item.get("arguments"))
            .and_then(Value::as_str);
        self.complete_tool_call(index, whole)
    }

    /// Keep a finished reasoning item that carries its encrypted content, so
    /// it can be sent back with the next request. Only the fields an input
    /// reasoning item accepts are kept.
    fn keep_reasoning(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("reasoning")
            || item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_none()
        {
            return;
        }
        let mut kept = serde_json::Map::new();
        for field in ["type", "id", "summary", "encrypted_content"] {
            if let Some(value) = item.get(field) {
                kept.insert(field.to_owned(), value.clone());
            }
        }
        self.queue.push_back(ModelEvent::Reasoning {
            state: super::tag_reasoning(&self.adapter, &self.model, Value::Object(kept)),
        });
    }

    fn slot(&mut self, index: usize) -> &mut Option<ToolCall> {
        if index >= self.calls.len() {
            self.calls.resize_with(index + 1, || None);
        }
        &mut self.calls[index]
    }

    /// Open the slot for a function call the model began at `index`. Any
    /// other output item, a message or reasoning, starts nothing here.
    fn start_tool_call(&mut self, index: usize, value: &Value) {
        let Some(item) = value.get("item") else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        self.saw_tool_call = true;
        let id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(|wire| {
                self.tool_names
                    .get(wire)
                    .cloned()
                    .unwrap_or_else(|| wire.to_owned())
            })
            .unwrap_or_default();
        *self.slot(index) = Some(ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: String::new(),
        });
        self.queue
            .push_back(ModelEvent::ToolCallStarted { index, id, name });
    }

    fn decode(&mut self, payload: &str) -> Result<(), ProviderError> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
            .unwrap_or(0);

        if let Some(text) = reasoning_text(kind, &value) {
            self.queue.push_back(ModelEvent::ThinkingDelta { text });
            return Ok(());
        }
        match kind {
            "response.output_text.delta" => {
                if let Some(text) = delta_str(&value) {
                    self.queue.push_back(ModelEvent::TextDelta { text });
                }
            }
            "response.output_item.added" => self.start_tool_call(index, &value),
            "response.function_call_arguments.delta" => {
                if let Some(fragment) = delta_str(&value).filter(|fragment| !fragment.is_empty()) {
                    if let Some(call) = self.slot(index) {
                        call.arguments.push_str(&fragment);
                    }
                    self.queue
                        .push_back(ModelEvent::ToolCallDelta { index, fragment });
                }
            }
            "response.output_item.done" => self.finish_item(index, value.get("item"))?,
            "response.failed" | "error" => {
                let error = value
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .or_else(|| value.get("error"))
                    // A bare `error` event carries its code and message on
                    // the event itself rather than under `error`.
                    .unwrap_or(&value)
                    .clone();
                return Err(super::openai::normalize_stream_error(&error));
            }
            "response.incomplete" => {
                self.stop = Some(StopReason::MaxTokens);
            }
            "response.completed" => {
                if let Some(usage) = value
                    .get("response")
                    .and_then(|response| response.get("usage"))
                    .filter(|usage| !usage.is_null())
                {
                    self.queue.push_back(ModelEvent::Usage {
                        input_tokens: count(usage, "input_tokens"),
                        output_tokens: count(usage, "output_tokens"),
                    });
                }
                self.stop.get_or_insert(if self.saw_tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// The only place a tool call becomes executable. `whole` is the complete
    /// argument string when the `done` event carried it; otherwise the
    /// fragments accumulated from the deltas are used.
    fn complete_tool_call(
        &mut self,
        index: usize,
        whole: Option<&str>,
    ) -> Result<(), ProviderError> {
        let Some(call) = self.calls.get_mut(index).and_then(Option::take) else {
            // A `done` for something that was not a function call (a message,
            // reasoning). Nothing to finish.
            return Ok(());
        };
        if call.name.is_empty() {
            return Err(ProviderError::Decode(format!(
                "tool call {index} never named a function"
            )));
        }
        let raw = whole.unwrap_or(&call.arguments);
        let raw = if raw.trim().is_empty() { "{}" } else { raw };
        let arguments = serde_json::from_str(raw).map_err(|error| {
            ProviderError::Decode(format!(
                "tool arguments for call {index} never completed: {error}"
            ))
        })?;
        self.queue.push_back(ModelEvent::ToolCallCompleted {
            index,
            id: call.id,
            name: call.name,
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
            if payload.is_empty() || payload == "[DONE]" {
                if payload == "[DONE]" {
                    self.done = true;
                    return self
                        .stop
                        .take()
                        .map(|stop| Ok(ModelEvent::Completed { stop }));
                }
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

/// The reasoning text an event carries, if it is a reasoning event.
///
/// Each summary part is its own paragraph — usually a bold title and a
/// sentence — so the end of one yields a paragraph break. Without it the next
/// part's opening `**` lands against the last one's closing `**`, and the
/// reasoning reads as one run of `****`-joined text.
fn reasoning_text(kind: &str, value: &Value) -> Option<String> {
    match kind {
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            delta_str(value)
        }
        "response.reasoning_summary_part.done" => Some("\n\n".to_owned()),
        _ => None,
    }
}

fn delta_str(value: &Value) -> Option<String> {
    value
        .get("delta")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn count(usage: &Value, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::IdempotencyKey;
    use crate::provider::{wire::WireResponse, ModelKey};

    struct Canned(std::sync::Mutex<Option<WireResponse>>);

    impl WireTransport for Canned {
        fn send(&self, _request: WireRequest) -> Result<WireResponse, ProviderError> {
            Ok(self.0.lock().unwrap().take().expect("one send"))
        }
    }

    /// A user message carrying tool results and text — the operator steering
    /// a running turn — sends every output before the text, so nothing sits
    /// between a `function_call` and its `function_call_output`.
    #[test]
    fn tool_outputs_come_before_text_in_the_same_user_message() {
        let mut items = Vec::new();
        encode_message(
            &ModelMessage {
                role: ModelRole::User,
                content: vec![
                    ModelContent::ToolResult {
                        id: "call-1".to_owned(),
                        content: "listing".to_owned(),
                        is_error: false,
                    },
                    ModelContent::Text {
                        text: "compare with rdb too".to_owned(),
                    },
                ],
            },
            &mut items,
            "codex",
            "gpt",
        );
        let kinds: Vec<&str> = items
            .iter()
            .map(|item| item["type"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(kinds, ["function_call_output", "message"]);
    }

    fn sse(status: u16, body: &str) -> Canned {
        let lines: Vec<Result<String, String>> =
            body.lines().map(|line| Ok(line.to_owned())).collect();
        Canned(std::sync::Mutex::new(Some(WireResponse {
            status,
            headers: vec![],
            lines: Box::new(lines.into_iter()),
        })))
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            model: ModelKey {
                provider: "codex-oauth".to_owned(),
                model: "gpt-5-codex".to_owned(),
            },
            system: None,
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: vec![],
            max_output_tokens: 256,
            effort: Some(crate::provider::Effort::Medium),
            idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
        }
    }

    #[test]
    fn encrypted_reasoning_is_requested_kept_and_sent_back_to_the_same_model_only() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"status\":\"completed\",\"summary\":[],\"encrypted_content\":\"ENC\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(200, body),
        )
        .with_id("codex-oauth");
        let state = provider
            .stream(&request())
            .unwrap()
            .find_map(|event| match event {
                Ok(ModelEvent::Reasoning { state }) => Some(state),
                _ => None,
            })
            .expect("the reasoning item is kept");
        let item = crate::provider::reasoning_payload(&state, "codex-oauth", "gpt-5-codex")
            .expect("tagged with this adapter and model");
        assert_eq!(item["encrypted_content"], "ENC");
        assert!(item.get("status").is_none(), "only input fields are kept");

        let mut next = request();
        next.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![
                ModelContent::Reasoning {
                    state: state.clone(),
                },
                ModelContent::ToolCall {
                    id: "c1".to_owned(),
                    name: "ls".to_owned(),
                    arguments: json!({}),
                },
            ],
        });
        let wire: Value = serde_json::from_str(&provider.encode(&next).body).unwrap();
        assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
        let kinds: Vec<&str> = wire["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("message"))
            .collect();
        assert_eq!(kinds, ["message", "reasoning", "function_call"]);

        // Another model cannot read it, so it is left out rather than sent.
        next.model.model = "gpt-5.5".to_owned();
        let wire: Value = serde_json::from_str(&provider.encode(&next).body).unwrap();
        assert!(!wire["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning"));
    }

    #[test]
    fn the_body_is_responses_shaped() {
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test/codex",
            ApiKey::new("t"),
            sse(200, ""),
        );
        let mut request = request();
        request.tools.push(ToolSchema {
            name: "fs.read".to_owned(),
            description: "read".to_owned(),
            input_schema: json!({"type": "object"}),
        });
        let wire = provider.encode(&request);
        let body: Value = serde_json::from_str(&wire.body).unwrap();
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["instructions"], json!(DEFAULT_INSTRUCTIONS));
        assert_eq!(body["parallel_tool_calls"], json!(false));
        assert_eq!(body["reasoning"]["effort"], json!("medium"));
        assert_eq!(body["input"][0]["type"], json!("message"));
        assert_eq!(body["input"][0]["content"][0]["type"], json!("input_text"));
        assert!(wire.headers.iter().any(|(key, _)| key == "originator"));
    }

    /// Any dotted tool the request offered comes back under its own name,
    /// not only the built-ins someone listed: `mcp.load` went out as
    /// `mcp_load` and, unmapped, failed every call.
    #[test]
    fn a_dotted_tool_name_maps_back_from_the_wire() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"mcp_load\"}}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"mcp_load\",\"arguments\":\"{\\\"server\\\":\\\"jira\\\"}\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n",
        );
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(200, body),
        );
        let mut asked = request();
        for name in ["mcp.load", "code.explain", "bash"] {
            asked.tools.push(crate::provider::ToolSchema {
                name: name.to_owned(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            });
        }
        let called: Vec<String> = provider
            .stream(&asked)
            .unwrap()
            .filter_map(|event| match event {
                Ok(ModelEvent::ToolCallCompleted { name, .. }) => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(called, ["mcp.load"]);
    }

    /// Two reasoning summary parts are two paragraphs, not one run of text
    /// with the second title's `**` against the first one's.
    #[test]
    fn reasoning_summary_parts_are_separated() {
        let body = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Planning**\"}\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\"}\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Inspecting**\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n",
        );
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(200, body),
        );
        let thinking: String = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .filter_map(|event| match event {
                ModelEvent::ThinkingDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "**Planning**\n\n**Inspecting**");
    }

    #[test]
    fn a_stream_yields_text_reasoning_a_tool_call_then_completed() {
        let body = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"ls\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\\\".\\\"}\"}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"ls\",\"arguments\":\"{\\\"path\\\":\\\".\\\"}\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
        );
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(200, body),
        );
        let events: Vec<ModelEvent> = provider
            .stream(&request())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(matches!(&events[0], ModelEvent::ThinkingDelta { text } if text == "think"));
        assert!(matches!(&events[1], ModelEvent::TextDelta { text } if text == "Hel"));
        assert!(matches!(&events[2], ModelEvent::TextDelta { text } if text == "lo"));
        assert!(matches!(
            &events[3],
            ModelEvent::ToolCallStarted { name, .. } if name == "ls"
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallCompleted { name, .. } if name == "ls"
        )));
        assert!(matches!(
            events.last().unwrap(),
            ModelEvent::Completed {
                stop: StopReason::ToolUse
            }
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage {
                input_tokens: 10,
                ..
            }
        )));
    }

    #[test]
    fn a_failed_event_becomes_a_provider_error() {
        let body = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}}\n";
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(200, body),
        );
        let last = provider.stream(&request()).unwrap().last().unwrap();
        assert!(matches!(last, Err(ProviderError::RateLimited { .. })));
    }

    #[test]
    fn a_request_at_fault_is_rejected_not_retried_whether_named_by_type_or_code() {
        for body in [
            // `response.failed` naming the class only in `code`.
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}}}\n",
            // A bare `error` event, code and message on the event itself.
            "data: {\"type\":\"error\",\"code\":\"invalid_prompt\",\"message\":\"Request blocked\"}\n",
        ] {
            let provider = OpenAiResponsesProvider::with_base_url(
                "https://host.test",
                ApiKey::new("t"),
                sse(200, body),
            );
            let last = provider.stream(&request()).unwrap().last().unwrap();
            assert!(
                matches!(&last, Err(ProviderError::InvalidRequest(message)) if message != "stream error"),
                "{body}: {last:?}"
            );
        }
    }

    #[test]
    fn a_non_200_is_normalized() {
        let provider = OpenAiResponsesProvider::with_base_url(
            "https://host.test",
            ApiKey::new("t"),
            sse(401, "{\"error\":{\"message\":\"bad token\"}}"),
        );
        assert!(matches!(
            provider.stream(&request()),
            Err(ProviderError::Auth(_))
        ));
    }
}
