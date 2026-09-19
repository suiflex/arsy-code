//! The provider boundary as an outside caller sees it: canonical request in,
//! normalized events out, with no Anthropic vocabulary crossing the line.

use arsy_kernel::{
    protocol::IdempotencyKey,
    provider::{
        anthropic::{AnthropicProvider, ApiKey, WireRequest, WireResponse, WireTransport},
        stream_with_retry, CanonicalModelRequest, Effort, ModelContent, ModelEvent,
        ModelEventStream, ModelKey, ModelMessage, ModelProvider, ModelRole, ProviderDescriptor,
        ProviderError, StopReason, ToolSchema,
    },
    secret::{Redactor, SecretHandle},
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// Replays a canned response and records the request it was given.
struct FakeTransport {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<&'static str>,
    sent: Arc<Mutex<Vec<String>>>,
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
}

impl WireTransport for FakeTransport {
    fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
        self.sent.lock().unwrap().push(format!(
            "{} {} {}",
            request.url,
            request
                .headers
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(";"),
            request.body
        ));
        Ok(WireResponse {
            status: self.status,
            headers: self.headers.clone(),
            lines: Box::new(
                self.body
                    .clone()
                    .into_iter()
                    .map(|line| Ok(line.to_owned())),
            ),
        })
    }
}

fn request(tools: Vec<ToolSchema>) -> CanonicalModelRequest {
    CanonicalModelRequest {
        model: ModelKey {
            provider: "anthropic".to_owned(),
            model: "claude-opus-5".to_owned(),
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

/// A tool call split across three fragments: none of them is valid JSON, and
/// only the completed call is executable.
#[test]
fn partial_tool_arguments_are_streamed_but_never_executable_until_complete() {
    let transport = FakeTransport::streaming(vec![
        r#"event: message_start"#,
        r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"fs.read"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"Cargo"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":".toml\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":12,"output_tokens":34}}"#,
        r#"data: {"type":"message_stop"}"#,
    ]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);

    let events = collect(provider.stream(&request(vec![read_tool()])).unwrap());

    assert_eq!(
        events,
        vec![
            ModelEvent::ToolCallStarted {
                index: 0,
                id: "toolu_1".to_owned(),
                name: "fs.read".to_owned(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                fragment: "{\"pa".to_owned(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                fragment: "th\":\"Cargo".to_owned(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                fragment: ".toml\"}".to_owned(),
            },
            ModelEvent::ToolCallCompleted {
                index: 0,
                id: "toolu_1".to_owned(),
                name: "fs.read".to_owned(),
                arguments: json!({"path": "Cargo.toml"}),
            },
            ModelEvent::Usage {
                input_tokens: 12,
                output_tokens: 34,
            },
            ModelEvent::Completed {
                stop: StopReason::ToolUse,
            },
        ]
    );
}

/// A thinking block streams its text as display-only deltas, before the answer
/// the reasoning produced.
#[test]
fn thinking_blocks_stream_as_display_only_deltas() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"consider the layout"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
    ]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);

    let events = collect(provider.stream(&request(Vec::new())).unwrap());

    assert_eq!(
        events,
        vec![
            ModelEvent::ThinkingDelta {
                text: "consider the layout".to_owned(),
            },
            ModelEvent::TextDelta {
                text: "answer".to_owned(),
            },
            ModelEvent::Completed {
                stop: StopReason::EndTurn
            },
        ],
        "a signature authenticates the block without becoming thinking text"
    );
}

/// A stream cut off mid-arguments must not produce an executable call.
#[test]
fn a_truncated_tool_call_yields_no_completed_call() {
    let transport = FakeTransport::streaming(vec![
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"fs.read"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
    ]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);

    let events: Vec<_> = provider
        .stream(&request(vec![read_tool()]))
        .unwrap()
        .collect();

    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Decode(message)))
            if message.contains("never completed")
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::ToolCallCompleted { .. }))),
        "an incomplete argument buffer must never become an executable call"
    );
}

#[test]
fn the_adapter_owns_wire_format_and_authentication() {
    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);
    let encoded = provider.encode(&request(vec![read_tool()]));

    assert_eq!(encoded.url, "https://example.test/v1/messages");
    assert!(encoded
        .headers
        .contains(&("x-api-key".to_owned(), "sk-test".to_owned())));
    assert!(encoded
        .headers
        .iter()
        .any(|(name, value)| name == "anthropic-version"
            && value == arsy_kernel::provider::anthropic::API_VERSION));

    let body: serde_json::Value = serde_json::from_str(&encoded.body).unwrap();
    assert_eq!(body["model"], "claude-opus-5");
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"], "be terse");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(
        body["tools"][0]["name"], "fs_read",
        "a dotted tool name is sanitized to match Anthropic's tool-name pattern"
    );

    // The credential is not reachable through a formatter.
    assert_eq!(format!("{:?}", ApiKey::new("sk-test")), "ApiKey(redacted)");
}

#[test]
fn a_dotted_tool_name_is_sanitized_for_any_credential_and_the_reply_maps_it_back() {
    // Anthropic's `tools[].name` pattern (`^[a-zA-Z0-9_-]{1,128}$`) applies
    // to every request, not just an OAuth-authenticated one: a dotted name
    // like `fs.read` is rejected outright unless it is sanitized first.
    let transport = FakeTransport::streaming(vec![
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"fs_read"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
    ]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);
    let events = collect(provider.stream(&request(vec![read_tool()])).unwrap());
    assert_eq!(
        events,
        vec![
            ModelEvent::ToolCallStarted {
                index: 0,
                id: "call-1".to_owned(),
                name: "fs.read".to_owned(),
            },
            ModelEvent::ToolCallCompleted {
                index: 0,
                id: "call-1".to_owned(),
                name: "fs.read".to_owned(),
                arguments: json!({}),
            },
        ],
        "the sanitized wire name is mapped back to the caller's own dotted name"
    );
}

#[test]
fn an_oauth_credential_authenticates_as_bearer_with_no_api_key_header() {
    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport)
            .with_oauth();
    let encoded = provider.encode(&request(Vec::new()));
    assert!(encoded
        .headers
        .contains(&("authorization".to_owned(), "Bearer sk-test".to_owned())));
    assert!(
        !encoded.headers.iter().any(|(name, _)| name == "x-api-key"),
        "an OAuth token must never ride in x-api-key: {:?}",
        encoded.headers
    );
    let beta = encoded
        .headers
        .iter()
        .find(|(name, _)| name == "anthropic-beta")
        .map(|(_, value)| value.as_str())
        .expect("anthropic-beta header present");
    for flag in ["oauth-2025-04-20", "claude-code-20250219"] {
        assert!(beta.contains(flag), "{beta} missing {flag}");
    }

    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let plain =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);
    let plain_encoded = plain.encode(&request(Vec::new()));
    assert!(plain_encoded
        .headers
        .contains(&("x-api-key".to_owned(), "sk-test".to_owned())));
    assert!(!plain_encoded
        .headers
        .iter()
        .any(|(name, _)| name == "anthropic-beta" || name == "authorization"));
}

#[test]
fn an_oauth_request_opens_with_the_claude_code_identity_ahead_of_the_real_system_prompt() {
    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport)
            .with_oauth();
    let body: serde_json::Value =
        serde_json::from_str(&provider.encode(&request(Vec::new())).body).unwrap();
    let system = body["system"]
        .as_array()
        .expect("system is an array of blocks under oauth");
    assert_eq!(
        system[0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(
        system[1]["text"], "be terse",
        "the real system prompt follows it"
    );

    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let plain =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport);
    let plain_body: serde_json::Value =
        serde_json::from_str(&plain.encode(&request(Vec::new())).body).unwrap();
    assert_eq!(
        plain_body["system"], "be terse",
        "a plain API key sends the system prompt as-is, not as blocks"
    );
}

#[test]
fn an_oauth_request_remaps_a_colliding_tool_name_and_the_reply_maps_it_back() {
    let bash = ToolSchema {
        name: "bash".to_owned(),
        description: "run a command".to_owned(),
        input_schema: json!({"type": "object", "properties": {}}),
    };

    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport)
            .with_oauth();
    let body: serde_json::Value =
        serde_json::from_str(&provider.encode(&request(vec![bash.clone()])).body).unwrap();
    assert_eq!(
        body["tools"][0]["name"], "Bash",
        "a name colliding with one of Claude Code's own tools is sent in its casing"
    );

    let transport = FakeTransport::streaming(vec![
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"Bash"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
    ]);
    let provider =
        AnthropicProvider::with_base_url("https://example.test", ApiKey::new("sk-test"), transport)
            .with_oauth();
    let events = collect(provider.stream(&request(vec![bash])).unwrap());
    assert_eq!(
        events,
        vec![
            ModelEvent::ToolCallStarted {
                index: 0,
                id: "call-1".to_owned(),
                name: "bash".to_owned(),
            },
            ModelEvent::ToolCallCompleted {
                index: 0,
                id: "call-1".to_owned(),
                name: "bash".to_owned(),
                arguments: json!({}),
            },
        ],
        "Claude Code's casing in the reply is mapped back to what the caller asked for"
    );
}

#[test]
fn model_call_redacts_registered_credentials_before_the_wire() {
    let transport = FakeTransport::streaming(vec![r#"data: {"type":"message_stop"}"#]);
    let sent = Arc::clone(&transport.sent);
    let handle = SecretHandle::new("os", "anthropic").unwrap();
    let mut redactor = Redactor::new();
    redactor.register(&handle, "super-secret-key").unwrap();
    let provider = AnthropicProvider::with_base_url(
        "https://example.test",
        ApiKey::new("wire-key"),
        transport,
    )
    .with_redactor(redactor);
    let mut request = request(Vec::new());
    request.messages[0].content = vec![ModelContent::Text {
        text: "never send super-secret-key".to_owned(),
    }];

    let _ = provider.stream(&request).unwrap().count();
    let wire = sent.lock().unwrap();
    assert!(!wire[0].contains("super-secret-key"));
    assert!(wire[0].contains("[redacted:secret://os/anthropic]"));
}

#[test]
fn http_failures_normalize_with_the_provider_retry_hint() {
    let cases = [
        (
            401u16,
            r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        ),
        (
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
        (
            529,
            r#"{"error":{"type":"overloaded_error","message":"overloaded"}}"#,
        ),
    ];
    let mut normalized = Vec::new();
    for (status, body) in cases {
        let transport = FakeTransport {
            status,
            headers: vec![("retry-after".to_owned(), "9".to_owned())],
            body: vec![body],
            sent: Arc::new(Mutex::new(Vec::new())),
        };
        let provider = AnthropicProvider::with_base_url(
            "https://example.test",
            ApiKey::new("sk-test"),
            transport,
        );
        let Err(error) = provider.stream(&request(Vec::new())) else {
            panic!("http {status} must not produce a stream");
        };
        normalized.push(error);
    }

    assert_eq!(
        normalized[0],
        ProviderError::Auth("authentication_error: invalid x-api-key".to_owned())
    );
    assert_eq!(normalized[0].retry_after(0), None, "auth is deterministic");
    assert_eq!(
        normalized[1],
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(9)),
        }
    );
    assert_eq!(
        normalized[1].retry_after(3),
        Some(Duration::from_secs(9)),
        "the provider hint wins over the default backoff"
    );
    assert_eq!(
        normalized[2],
        ProviderError::Server {
            status: 529,
            message: "overloaded_error: overloaded".to_owned(),
        }
    );
    assert!(normalized[2].retry_after(0).is_some());
}

/// A second provider is a new impl and nothing else: the same caller drives it
/// through the same trait, with no operation-side change.
#[test]
fn a_second_provider_needs_no_change_above_the_trait() {
    struct EchoProvider(ProviderDescriptor);

    impl ModelProvider for EchoProvider {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.0
        }

        fn stream(
            &self,
            request: &CanonicalModelRequest,
        ) -> Result<ModelEventStream, ProviderError> {
            let text = format!("{}", request.model);
            Ok(Box::new(
                [
                    Ok(ModelEvent::TextDelta { text }),
                    Ok(ModelEvent::Completed {
                        stop: StopReason::EndTurn,
                    }),
                ]
                .into_iter(),
            ))
        }
    }

    let anthropic = AnthropicProvider::with_base_url(
        "https://example.test",
        ApiKey::new("sk-test"),
        FakeTransport::streaming(vec![
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ]),
    );
    let echo = EchoProvider(ProviderDescriptor {
        id: "echo".to_owned(),
        max_retries: 0,
    });

    let providers: [&dyn ModelProvider; 2] = [&anthropic, &echo];
    for provider in providers {
        let events = collect(
            stream_with_retry(provider, &request(Vec::new()), &mut |_| {
                panic!("no retry expected")
            })
            .unwrap(),
        );
        assert!(matches!(events.first(), Some(ModelEvent::TextDelta { .. })));
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Completed {
                stop: StopReason::EndTurn
            })
        );
    }
}

fn collect(stream: ModelEventStream) -> Vec<ModelEvent> {
    stream.map(Result::unwrap).collect()
}

/// The Messages dialect spends reasoning out of the output budget, so the level
/// becomes a token count. An unset effort must leave the body exactly as it was
/// before the knob existed.
#[test]
fn effort_becomes_a_thinking_budget_inside_the_output_budget() {
    const THINKING_FLOOR: u32 = 1024;

    let provider = AnthropicProvider::with_base_url(
        "https://example.test",
        ApiKey::new("sk-test"),
        FakeTransport::streaming(Vec::new()),
    );

    let unset: serde_json::Value =
        serde_json::from_str(&provider.encode(&request(Vec::new())).body).unwrap();
    assert!(
        unset.get("thinking").is_none(),
        "an unset effort sends no thinking block"
    );

    // 256 output tokens cannot hold the 1024-token floor and still leave room
    // to answer, so the request goes out without a budget it would be rejected
    // for.
    for level in Effort::ALL {
        let small = CanonicalModelRequest {
            effort: Some(level),
            ..request(Vec::new())
        };
        let body: serde_json::Value = serde_json::from_str(&provider.encode(&small).body).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "{level} fit a budget into 256 output tokens"
        );
    }

    let budget = |level: Effort| -> u64 {
        let large = CanonicalModelRequest {
            effort: Some(level),
            max_output_tokens: 20_000,
            ..request(Vec::new())
        };
        let body: serde_json::Value = serde_json::from_str(&provider.encode(&large).body).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        body["thinking"]["budget_tokens"].as_u64().unwrap()
    };

    // The floor fitting is not the same as an answer fitting: a budget must
    // leave at least as much room to answer as it takes to think.
    let smallest_with_thinking = THINKING_FLOOR * 2;
    for (max_tokens, expected) in [
        (smallest_with_thinking - 1, None),
        (smallest_with_thinking, Some(THINKING_FLOOR)),
    ] {
        let body: serde_json::Value = serde_json::from_str(
            &provider
                .encode(&CanonicalModelRequest {
                    effort: Some(Effort::Low),
                    max_output_tokens: max_tokens,
                    ..request(Vec::new())
                })
                .body,
        )
        .unwrap();
        assert_eq!(
            body.get("thinking")
                .map(|thinking| thinking["budget_tokens"].as_u64().unwrap() as u32),
            expected,
            "at max_tokens {max_tokens}"
        );
    }

    assert_eq!(budget(Effort::Low), 5_000);
    assert_eq!(budget(Effort::Medium), 10_000);
    assert_eq!(budget(Effort::High), 16_000);
    for level in Effort::ALL {
        assert!(
            budget(level) < 20_000,
            "{level} left no output budget to answer with"
        );
    }
}
