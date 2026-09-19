//! Anthropic Messages adapter: wire format, authentication, error normalization.
//!
//! HTTP itself is injected as [`WireTransport`] rather than depended on, so the
//! part that differs per provider — body shape, headers, status mapping, SSE
//! semantics — is the part that is testable here.
//!
//! A Claude Code OAuth access token ([`with_oauth`](AnthropicProvider::with_oauth))
//! is not a drop-in replacement for a Console API key: Anthropic's server only
//! accepts one when the request also carries the right beta flags, opens with
//! the exact system-prompt identity string Claude Code itself sends, and
//! spells any tool name that collides with Claude Code's own tools in Claude
//! Code's casing. Get any of the three wrong and the whole request is
//! rejected — not just the part that looks related.

pub use super::wire::{ApiKey, WireRequest, WireResponse, WireTransport};
use super::{
    CanonicalModelRequest, ModelContent, ModelEvent, ModelEventStream, ModelProvider, ModelRole,
    ProviderDescriptor, ProviderError, StopReason,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
};

/// Wire version Anthropic requires on every request.
pub const API_VERSION: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Smallest thinking budget the Messages API accepts.
const THINKING_FLOOR: u32 = 1024;

/// Beta flags the Claude Code OAuth client sends together, verbatim, on
/// every request made with an OAuth-issued token. `oauth-2025-04-20` is
/// what lets a bearer token stand in for a Console API key at all;
/// `claude-code-20250219` is the compatibility flag the server checks the
/// system-prompt identity and tool names against — dropping it does not
/// relax those checks, it just removes the flag that explains why they are
/// being made. The other two match what the official client always sends
/// in this mode, matching official OAuth-consuming clients such as
/// OpenCode's `opencode-anthropic-auth`.
const CLAUDE_CODE_BETA: &str = "oauth-2025-04-20,claude-code-20250219,\
     interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";

/// The exact system-prompt block Claude Code itself sends first. An
/// OAuth-authenticated request without it is rejected even though the
/// error Anthropic returns for it does not say so.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Claude Code's own tool names. Anthropic's server validates tool names
/// against this set on an OAuth-authenticated request: a name that
/// collides case-insensitively (ARSY's `bash`, say) has to be sent in
/// exactly this casing or the request is rejected, and the reply has to be
/// mapped back to whatever casing the caller actually used. A name with no
/// collision here passes through untouched either way.
const CLAUDE_CODE_TOOL_NAMES: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Give `name` a wire spelling every Anthropic tool name has to match —
/// `^[a-zA-Z0-9_-]{1,128}$`, API key or OAuth token alike, so a dotted name
/// such as ARSY's `fs.read` does not qualify on its own — and, only under
/// OAuth, Claude Code's own casing when it collides case-insensitively with
/// one of Claude Code's tools.
fn wire_tool_name(name: &str, oauth: bool) -> Cow<'_, str> {
    let legal = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    let sanitized: Cow<str> = if legal {
        Cow::Borrowed(name)
    } else {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .into()
    };
    if !oauth {
        return sanitized;
    }
    match CLAUDE_CODE_TOOL_NAMES
        .iter()
        .find(|canonical| canonical.eq_ignore_ascii_case(&sanitized))
    {
        Some(canonical) => Cow::Borrowed(*canonical),
        None => sanitized,
    }
}

pub struct AnthropicProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    transport: T,
    redactor: Redactor,
    /// Set when `key` is a Claude Code OAuth access token rather than a
    /// Console API key. An OAuth-issued token has to be sent, encoded, and
    /// replied to differently in three places `encode` touches — see the
    /// module doc — not just given an extra header.
    oauth: bool,
}

impl<T: WireTransport> AnthropicProvider<T> {
    pub fn new(key: ApiKey, transport: T) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key, transport)
    }

    pub fn with_base_url(base_url: impl Into<String>, key: ApiKey, transport: T) -> Self {
        Self {
            descriptor: ProviderDescriptor {
                id: "anthropic".to_owned(),
                max_retries: 3,
            },
            base_url: base_url.into(),
            key,
            transport,
            redactor: Redactor::new(),
            oauth: false,
        }
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Mark `key` as a Claude Code OAuth access token, so every request
    /// carries the beta header Anthropic's OAuth client requires.
    pub fn with_oauth(mut self) -> Self {
        self.oauth = true;
        self
    }

    /// Canonical request to Anthropic Messages wire form.
    pub fn encode(&self, request: &CanonicalModelRequest) -> WireRequest {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(request.model.model));
        body.insert("max_tokens".to_owned(), json!(request.max_output_tokens));
        body.insert("stream".to_owned(), json!(true));

        // This dialect spends reasoning from the output budget, so the level is
        // a share of `max_tokens`. The budget has a floor of 1024, and thinking
        // that consumes the whole budget leaves nothing to answer with, so the
        // answer keeps at least that same floor and a request too small to hold
        // both carries no thinking block at all.
        if let Some(budget) = request.effort.and_then(|effort| {
            let (numerator, denominator) = effort.thinking_share();
            let share = request.max_output_tokens / denominator * numerator;
            let ceiling = request.max_output_tokens.checked_sub(THINKING_FLOOR)?;
            (ceiling >= THINKING_FLOOR).then(|| share.clamp(THINKING_FLOOR, ceiling))
        }) {
            body.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }

        if self.oauth {
            // An OAuth-authenticated request opens with Claude Code's own
            // identity, or Anthropic rejects it outright; the operator's
            // real system prompt (if any) follows as a second block.
            let mut blocks = vec![json!({
                "type": "text",
                "text": CLAUDE_CODE_IDENTITY,
                "cache_control": {"type": "ephemeral"},
            })];
            if let Some(system) = &request.system {
                blocks.push(json!({
                    "type": "text",
                    "text": system,
                    "cache_control": {"type": "ephemeral"},
                }));
            }
            body.insert("system".to_owned(), Value::Array(blocks));
        } else if let Some(system) = &request.system {
            body.insert("system".to_owned(), json!(system));
        }
        body.insert(
            "messages".to_owned(),
            Value::Array(
                request
                    .messages
                    .iter()
                    .map(|message| encode_message(message, self.oauth))
                    .collect(),
            ),
        );
        if !request.tools.is_empty() {
            body.insert(
                "tools".to_owned(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            let name = wire_tool_name(&tool.name, self.oauth);
                            json!({
                                "name": name,
                                "description": tool.description,
                                "input_schema": tool.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
        }
        let mut headers = vec![
            ("anthropic-version".to_owned(), API_VERSION.to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
        ];
        if self.oauth {
            // Anthropic rejects an OAuth-issued token sent as `x-api-key`
            // outright; it has to ride as a bearer token instead, with no
            // `x-api-key` header present at all.
            headers.push((
                "authorization".to_owned(),
                format!("Bearer {}", self.key.expose()),
            ));
            headers.push(("anthropic-beta".to_owned(), CLAUDE_CODE_BETA.to_owned()));
        } else {
            headers.push(("x-api-key".to_owned(), self.key.expose().to_owned()));
        }
        WireRequest {
            url: format!("{}/v1/messages", self.base_url.trim_end_matches('/')),
            headers,
            body: Value::Object(body).to_string(),
        }
    }
}

fn encode_message(message: &super::ModelMessage, oauth: bool) -> Value {
    json!({
        "role": match message.role {
            ModelRole::User => "user",
            ModelRole::Assistant => "assistant",
        },
        "content": message
            .content
            .iter()
            .map(|content| encode_content(content, oauth))
            .collect::<Vec<_>>(),
    })
}

fn encode_content(content: &ModelContent, oauth: bool) -> Value {
    match content {
        ModelContent::Text { text } => json!({ "type": "text", "text": text }),
        ModelContent::ToolCall {
            id,
            name,
            arguments,
        } => {
            let name = wire_tool_name(name, oauth);
            json!({ "type": "tool_use", "id": id, "name": name, "input": arguments })
        }
        ModelContent::ToolResult {
            id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        }),
        ModelContent::Image { media_type, data } => json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }),
    }
}

impl<T: WireTransport> ModelProvider for AnthropicProvider<T> {
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
        // The wire echoes back whatever name `encode` sent; when that
        // differed from the caller's own name — a dotted name that had to
        // be sanitized, or Claude Code's spelling of a colliding tool under
        // OAuth — this maps it back to the name the caller actually asked
        // for. Empty when nothing differed, which costs the lookup nothing.
        let tool_names: HashMap<String, String> = request
            .tools
            .iter()
            .filter_map(|tool| {
                let mapped = wire_tool_name(&tool.name, self.oauth);
                (mapped != tool.name).then(|| (mapped.into_owned(), tool.name.clone()))
            })
            .collect();
        Ok(Box::new(EventDecoder::new(response.lines, tool_names)))
    }
}

/// HTTP failure to normalized error, including the provider's retry hint.
///
/// A non-200 response carries a JSON error body rather than an event stream, so
/// the same line iterator is drained as the body.
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
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("error");
    let detail = error.get("message").and_then(Value::as_str)?;
    Some(format!("{kind}: {detail}"))
}

/// Server-sent-event decoder for the Messages stream.
///
/// Tool arguments arrive as `input_json_delta` fragments that are only valid
/// JSON once concatenated. Fragments are surfaced as
/// [`ModelEvent::ToolCallDelta`] and buffered; the call becomes executable only
/// at `content_block_stop`, when the accumulated text parses. A stream that is
/// cut short therefore yields no executable call.
struct EventDecoder {
    lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
    blocks: Vec<ToolBlock>,
    /// One wire event can carry two canonical facts (usage and stop reason).
    queue: VecDeque<ModelEvent>,
    done: bool,
    /// Claude Code's spelling of a tool name -> what the caller actually
    /// named it. Empty outside OAuth mode, or when nothing collided.
    tool_names: HashMap<String, String>,
}

struct ToolBlock {
    id: String,
    name: String,
    arguments: String,
}

impl EventDecoder {
    fn new(
        lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
        tool_names: HashMap<String, String>,
    ) -> Self {
        Self {
            lines,
            blocks: Vec::new(),
            queue: VecDeque::new(),
            done: false,
            tool_names,
        }
    }

    /// Decode one `data:` payload into zero or more canonical events.
    fn decode(&mut self, payload: &str) -> Result<(), ProviderError> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Decode("event is missing `type`".to_owned()))?;
        match kind {
            "content_block_start" => self.block_start(&value),
            "content_block_delta" => self.block_delta(&value),
            "content_block_stop" => self.block_stop(&value),
            "message_delta" => {
                // One wire event reports usage and the stop reason together.
                if let Some(usage) = value.get("usage") {
                    self.queue.push_back(ModelEvent::Usage {
                        input_tokens: count(usage, "input_tokens"),
                        output_tokens: count(usage, "output_tokens"),
                    });
                }
                if let Some(stop) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .map(stop_reason)
                {
                    self.queue.push_back(ModelEvent::Completed { stop });
                }
                Ok(())
            }
            "message_stop" => {
                self.done = true;
                Ok(())
            }
            "error" => Err(normalize_stream_error(&value)),
            // `message_start`, `ping`, and anything added later.
            _ => Ok(()),
        }
    }

    /// Only `tool_use` blocks are tracked; text needs no per-block state.
    fn block_start(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let block = value
            .get("content_block")
            .ok_or_else(|| ProviderError::Decode("missing `content_block`".to_owned()))?;
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return Ok(());
        }
        let id = field(block, "id")?.to_owned();
        let wire_name = field(block, "name")?;
        let name = self
            .tool_names
            .get(wire_name)
            .cloned()
            .unwrap_or_else(|| wire_name.to_owned());
        if self.blocks.len() != index {
            return Err(ProviderError::Decode(format!(
                "content block {index} started out of order"
            )));
        }
        self.blocks.push(ToolBlock {
            id: id.clone(),
            name: name.clone(),
            arguments: String::new(),
        });
        self.queue
            .push_back(ModelEvent::ToolCallStarted { index, id, name });
        Ok(())
    }

    fn block_delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let delta = value
            .get("delta")
            .ok_or_else(|| ProviderError::Decode("missing `delta`".to_owned()))?;
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => self.queue.push_back(ModelEvent::TextDelta {
                text: field(delta, "text")?.to_owned(),
            }),
            Some("thinking_delta") => self.queue.push_back(ModelEvent::ThinkingDelta {
                text: field(delta, "thinking")?.to_owned(),
            }),
            Some("input_json_delta") => {
                let fragment = field(delta, "partial_json")?.to_owned();
                let block = self.blocks.get_mut(index).ok_or_else(|| {
                    ProviderError::Decode(format!("delta for unstarted block {index}"))
                })?;
                block.arguments.push_str(&fragment);
                self.queue
                    .push_back(ModelEvent::ToolCallDelta { index, fragment });
            }
            // Signature deltas authenticate a thinking block without being
            // part of it; anything else carries nothing canonical.
            _ => {}
        }
        Ok(())
    }

    /// The only place a tool call becomes executable: the buffered fragments
    /// must parse as a whole, or the call is rejected rather than guessed at.
    fn block_stop(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let Some(block) = self.blocks.get(index) else {
            return Ok(());
        };
        // An empty-input tool call is legal and encodes as `{}`.
        let raw = if block.arguments.trim().is_empty() {
            "{}"
        } else {
            &block.arguments
        };
        let arguments = serde_json::from_str(raw).map_err(|error| {
            ProviderError::Decode(format!(
                "tool arguments for block {index} never completed: {error}"
            ))
        })?;
        self.queue.push_back(ModelEvent::ToolCallCompleted {
            index,
            id: block.id.clone(),
            name: block.name.clone(),
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
            let line = match self.lines.next()? {
                Ok(line) => line,
                Err(error) => {
                    self.done = true;
                    return Some(Err(ProviderError::Transport(error)));
                }
            };
            let Some(payload) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            if let Err(error) = self.decode(payload.trim()) {
                self.done = true;
                self.queue.clear();
                return Some(Err(error));
            }
        }
    }
}

fn normalize_stream_error(value: &Value) -> ProviderError {
    let error = value.get("error");
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("stream error")
        .to_owned();
    match kind {
        "authentication_error" | "permission_error" => ProviderError::Auth(message),
        "invalid_request_error" => ProviderError::InvalidRequest(message),
        "not_found_error" => ProviderError::NotFound(message),
        "rate_limit_error" => ProviderError::RateLimited { retry_after: None },
        "overloaded_error" => ProviderError::Server {
            status: 529,
            message,
        },
        _ => ProviderError::Server {
            status: 500,
            message,
        },
    }
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

fn block_index(value: &Value) -> Result<usize, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| ProviderError::Decode("event is missing `index`".to_owned()))
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, ProviderError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Decode(format!("event is missing `{name}`")))
}

fn count(usage: &Value, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or_default()
}
