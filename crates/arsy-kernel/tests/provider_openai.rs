//! The OpenAI-compatible adapter as an outside caller sees it: canonical
//! request in, the same normalized events the Anthropic adapter produces out.

use arsy_kernel::{
    protocol::IdempotencyKey,
    provider::{
        openai::OpenAiProvider,
        wire::{ApiKey, WireRequest, WireResponse, WireTransport},
        CanonicalModelRequest, Effort, ModelContent, ModelEvent, ModelEventStream, ModelKey,
        ModelMessage, ModelProvider, ModelRole, ProviderError, StopReason, ToolSchema,
    },
    secret::{Redactor, SecretHandle},
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Replays a canned response and records the request it was given.
struct FakeTransport {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<&'static str>,
    sent: Arc<Mutex<Vec<WireRequest>>>,
}

impl FakeTransport {
    fn streaming(body: Vec<&'static str>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn failing(status: u16, headers: Vec<(String, String)>, body: &'static str) -> Self {
        Self {
            status,
            headers,
            body: vec![body],
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl WireTransport for FakeTransport {
    fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
        let lines = self.body.clone();
        self.sent.lock().unwrap().push(request);
        Ok(WireResponse {
            status: self.status,
            headers: self.headers.clone(),
            lines: Box::new(lines.into_iter().map(|line| Ok(line.to_owned()))),
        })
    }
}

fn request(tools: Vec<ToolSchema>) -> CanonicalModelRequest {
    CanonicalModelRequest {
        model: ModelKey {
            provider: "local".to_owned(),
            model: "qwen3-coder".to_owned(),
        },
        system: Some("be terse".to_owned()),
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "read Cargo.toml".to_owned(),
            }],
        }],
        tools,
        max_output_tokens: 256,
        effort: None,
        idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
    }
}

fn read_tool() -> ToolSchema {
    ToolSchema {
        name: "fs.read".to_owned(),
        description: "read a file".to_owned(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

fn collect(stream: ModelEventStream) -> Vec<ModelEvent> {
    stream.map(Result::unwrap).collect()
}

/// A stream is not `Debug`, so an expected failure is unwrapped by hand.
fn error(result: Result<ModelEventStream, ProviderError>) -> ProviderError {
    match result {
        Ok(_) => panic!("expected the request to fail"),
        Err(error) => error,
    }
}

#[test]
fn a_request_carries_the_dialect_headers_and_folds_the_system_prompt_into_messages() {
    let transport = FakeTransport::streaming(vec!["data: [DONE]"]);
    let sent = Arc::clone(&transport.sent);
    // A trailing slash on the configured root must not double up in the path.
    let provider = OpenAiProvider::with_base_url(
        "http://localhost:11434/v1/",
        ApiKey::new("sk-test-value"),
        transport,
    );

    provider
        .stream(&request(vec![read_tool()]))
        .unwrap()
        .count();

    let sent = sent.lock().unwrap();
    let wire = &sent[0];
    assert_eq!(wire.url, "http://localhost:11434/v1/chat/completions");
    assert!(wire.headers.contains(&(
        "authorization".to_owned(),
        "Bearer sk-test-value".to_owned()
    )));
    assert!(wire
        .headers
        .contains(&("accept".to_owned(), "text/event-stream".to_owned())));

    let body: Value = serde_json::from_str(&wire.body).unwrap();
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["model"], json!("qwen3-coder"));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "read Cargo.toml"},
        ]),
        "this dialect has no system field, so the prompt becomes the first message"
    );
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("fs.read"));
    assert_eq!(body["parallel_tool_calls"], json!(false));
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["type"],
        json!("object"),
        "the tool schema is passed through untouched under the dialect's own key"
    );
}

#[test]
fn a_tool_result_becomes_its_own_message_after_the_call_that_produced_it() {
    let transport = FakeTransport::streaming(vec!["data: [DONE]"]);
    let sent = Arc::clone(&transport.sent);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport);

    let mut canonical = request(Vec::new());
    canonical.messages.push(ModelMessage {
        role: ModelRole::Assistant,
        content: vec![ModelContent::ToolCall {
            id: "call_1".to_owned(),
            name: "fs.read".to_owned(),
            arguments: json!({"path": "Cargo.toml"}),
        }],
    });
    canonical.messages.push(ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::ToolResult {
            id: "call_1".to_owned(),
            content: "no such file".to_owned(),
            is_error: true,
        }],
    });
    provider.stream(&canonical).unwrap().count();

    let sent = sent.lock().unwrap();
    let body: Value = serde_json::from_str(&sent[0].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["tool_calls"][0]["id"], json!("call_1"));
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        json!(r#"{"path":"Cargo.toml"}"#),
        "this dialect carries tool arguments as a JSON string, not an object"
    );
    assert_eq!(messages[3]["role"], json!("tool"));
    assert_eq!(messages[3]["tool_call_id"], json!("call_1"));
    assert_eq!(
        messages[3]["content"],
        json!("error: no such file"),
        "the dialect has no error flag, so the fact is carried in the content"
    );
}

/// A host that enforces the `^[a-zA-Z0-9_-]+$` name pattern sees `fs_read`,
/// and the call it returns is executed under the canonical `fs.read`.
#[test]
fn sanitized_tool_names_go_out_with_underscores_and_come_back_canonical() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"fs_read","arguments":"{}"}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"data: [DONE]"#,
    ]);
    let sent = Arc::clone(&transport.sent);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport)
        .with_sanitized_tool_names(true);

    let mut canonical = request(vec![read_tool()]);
    canonical.messages.push(ModelMessage {
        role: ModelRole::Assistant,
        content: vec![ModelContent::ToolCall {
            id: "call_1".to_owned(),
            name: "fs.read".to_owned(),
            arguments: json!({}),
        }],
    });
    let events = collect(provider.stream(&canonical).unwrap());

    let body: Value = serde_json::from_str(&sent.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], json!("fs_read"));
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["name"],
        json!("fs_read"),
        "a call replayed from history must match the declared tool name"
    );
    assert!(events.contains(&ModelEvent::ToolCallCompleted {
        index: 0,
        id: "call_2".to_owned(),
        name: "fs.read".to_owned(),
        arguments: json!({}),
    }));
}

/// A tool call split across fragments: none of them is valid JSON on its own,
/// and only the completed call is executable.
#[test]
fn partial_tool_arguments_are_streamed_but_never_executable_until_complete() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":""}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"Cargo"}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":".toml\"}"}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34}}"#,
        r#"data: [DONE]"#,
    ]);
    let provider = OpenAiProvider::with_base_url(
        "https://gateway.test/v1",
        ApiKey::new("sk-test-value"),
        transport,
    );

    let events = collect(provider.stream(&request(vec![read_tool()])).unwrap());

    let fragments: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::ToolCallDelta { fragment, .. } => Some(fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fragments, [r#"{"pa"#, r#"th":"Cargo"#, r#".toml"}"#]);
    for fragment in &fragments {
        assert!(
            serde_json::from_str::<Value>(fragment).is_err(),
            "{fragment} must not be executable on its own"
        );
    }
    assert!(events.contains(&ModelEvent::ToolCallStarted {
        index: 0,
        id: "call_1".to_owned(),
        name: "fs.read".to_owned(),
    }));
    assert!(events.contains(&ModelEvent::ToolCallCompleted {
        index: 0,
        id: "call_1".to_owned(),
        name: "fs.read".to_owned(),
        arguments: json!({"path": "Cargo.toml"}),
    }));
    assert!(events.contains(&ModelEvent::Usage {
        input_tokens: 12,
        output_tokens: 34,
    }));
    assert_eq!(
        events.last(),
        Some(&ModelEvent::Completed {
            stop: StopReason::ToolUse,
        })
    );
}

#[test]
fn a_stream_cut_short_yields_no_executable_call() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":"{\"pa"}}]}}]}"#,
    ]);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport);

    let events: Vec<_> = provider
        .stream(&request(vec![read_tool()]))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::ToolCallCompleted { .. })),
        "a truncated stream must not produce a runnable call: {events:?}"
    );
}

#[test]
fn arguments_that_never_parse_are_rejected_rather_than_guessed_at() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":"{\"path\""}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport);

    let last = provider
        .stream(&request(vec![read_tool()]))
        .unwrap()
        .last()
        .unwrap();

    assert!(matches!(last, Err(ProviderError::Decode(_))), "{last:?}");
}

#[test]
fn text_streams_as_deltas_and_a_length_stop_is_normalized() {
    let transport = FakeTransport::streaming(vec![
        // Some gateways send the error key on every chunk with a null value.
        // That is the absence of an error, not one.
        r#"data: {"error":null,"choices":[{"index":0,"delta":{"content":"he"}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"content":"llo"}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
        r#"data: [DONE]"#,
        r#"data: {"choices":[{"index":0,"delta":{"content":"after done"}}]}"#,
    ]);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport);

    let events = collect(provider.stream(&request(Vec::new())).unwrap());

    assert_eq!(
        events,
        [
            ModelEvent::TextDelta {
                text: "he".to_owned()
            },
            ModelEvent::TextDelta {
                text: "llo".to_owned()
            },
            ModelEvent::Completed {
                stop: StopReason::MaxTokens
            },
        ],
        "nothing after the [DONE] sentinel is decoded"
    );
}

/// Reasoning models surface their thinking in `reasoning_content` before the
/// answer it produced, so the verbose stream can show the two apart.
#[test]
fn reasoning_content_streams_as_thinking_deltas_before_the_answer() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"first thought"}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":null}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"content":"the answer"}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let provider = OpenAiProvider::new(ApiKey::new("sk-test-value"), transport);

    let events = collect(provider.stream(&request(Vec::new())).unwrap());

    assert_eq!(
        events,
        [
            ModelEvent::ThinkingDelta {
                text: "first thought".to_owned(),
            },
            ModelEvent::TextDelta {
                text: "the answer".to_owned(),
            },
            ModelEvent::Completed {
                stop: StopReason::EndTurn
            },
        ],
        "an empty or null reasoning field carries no thinking event"
    );
}

#[test]
fn statuses_and_stream_errors_map_onto_the_shared_error_classes() {
    /// Whether the normalized error is the class the status should produce.
    type Expected = fn(&ProviderError) -> bool;

    let cases: Vec<(u16, &'static str, Expected)> = vec![
        (
            401,
            r#"{"error":{"type":"invalid_request_error","message":"bad key"}}"#,
            |error| matches!(error, ProviderError::Auth(message) if message.contains("bad key")),
        ),
        (
            404,
            r#"{"error":{"type":"not_found_error","message":"no model"}}"#,
            |error| matches!(error, ProviderError::NotFound(_)),
        ),
        (
            400,
            r#"{"error":{"type":"invalid_request_error","message":"too long"}}"#,
            |error| matches!(error, ProviderError::InvalidRequest(_)),
        ),
        (503, "upstream down", |error| {
            matches!(error, ProviderError::Server { status: 503, .. })
        }),
    ];
    for (status, body, expected) in cases {
        let provider = OpenAiProvider::new(
            ApiKey::new("sk-test-value"),
            FakeTransport::failing(status, Vec::new(), body),
        );
        let error = error(provider.stream(&request(Vec::new())));
        assert!(expected(&error), "status {status} produced {error:?}");
    }

    let provider = OpenAiProvider::new(
        ApiKey::new("sk-test-value"),
        FakeTransport::failing(
            429,
            vec![("Retry-After".to_owned(), "7".to_owned())],
            r#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#,
        ),
    );
    assert_eq!(
        error(provider.stream(&request(Vec::new()))),
        ProviderError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(7)),
        },
        "the provider's own hint is preserved, and the header name's casing does not matter"
    );

    // A gateway may report failure mid-stream instead of as a status.
    let provider = OpenAiProvider::new(
        ApiKey::new("sk-test-value"),
        FakeTransport::streaming(vec![
            r#"data: {"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ]),
    );
    assert_eq!(
        provider.stream(&request(Vec::new())).unwrap().last(),
        Some(Err(ProviderError::RateLimited { retry_after: None }))
    );
}

#[test]
fn a_credential_cannot_reach_the_wire_body_through_the_prompt() {
    let handle = SecretHandle::new("os", "gateway").unwrap();
    let mut redactor = Redactor::new();
    redactor.register(&handle, "sk-live-value-1234").unwrap();

    let transport = FakeTransport::streaming(vec!["data: [DONE]"]);
    let sent = Arc::clone(&transport.sent);
    let provider =
        OpenAiProvider::new(ApiKey::new("sk-live-value-1234"), transport).with_redactor(redactor);

    let mut canonical = request(Vec::new());
    canonical.messages[0].content = vec![ModelContent::Text {
        text: "my key is sk-live-value-1234".to_owned(),
    }];
    provider.stream(&canonical).unwrap().count();

    let sent = sent.lock().unwrap();
    assert!(
        !sent[0].body.contains("sk-live-value-1234"),
        "a credential echoed into the prompt is redacted before it is sent"
    );
    assert!(
        sent[0]
            .headers
            .iter()
            .any(|(_, value)| value.contains("sk-live-value-1234")),
        "the authorization header is the one place the value legitimately appears"
    );
}

/// The Chat Completions dialect takes the level by name, so it goes on the wire
/// as written. An unset effort must leave the body as it was before the knob
/// existed, because most hosts on this route have no reasoning model at all.
#[test]
fn effort_travels_as_a_named_reasoning_level() {
    let provider = OpenAiProvider::with_base_url(
        "https://gateway.test/v1",
        ApiKey::new("sk-test"),
        FakeTransport::streaming(Vec::new()),
    );

    let unset: Value = serde_json::from_str(&provider.encode(&request(Vec::new())).body).unwrap();
    assert!(
        unset.get("reasoning_effort").is_none(),
        "an unset effort sends no reasoning field"
    );

    for level in Effort::ALL {
        let body: Value = serde_json::from_str(
            &provider
                .encode(&CanonicalModelRequest {
                    effort: Some(level),
                    ..request(Vec::new())
                })
                .body,
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], level.as_str());
        // The budget is the host's business on this route, so nothing else in
        // the body moves with the level.
        assert_eq!(body["max_tokens"], unset["max_tokens"]);
    }
}
