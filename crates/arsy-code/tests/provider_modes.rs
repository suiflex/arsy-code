//! Plan mode holds whichever provider a turn talks to.
//!
//! The approval modes are enforced by the harness, not by the adapters: an
//! adapter only encodes the tools it is handed and decodes the calls a model
//! makes. So the property worth pinning is the whole path, once per dialect —
//! the Plan-mode tool list is what reaches the wire, and a model that asks for
//! a write anyway (a tool it was never shown, spelled the way its dialect
//! spells it) is refused by the runtime and changes nothing on disk.

use arsy_code::{
    agent::{self, Authorization, ExecutionMode, ToolRuntime},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::{ArtifactStore, FileArtifactStore},
    capability::{CapabilityAction, PolicySource, ResourcePattern},
    domain::Principal,
    policy::{
        ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance,
        WorkspaceCleanliness,
    },
    protocol::IdempotencyKey,
    provider::{
        anthropic::AnthropicProvider,
        google_code_assist::GoogleCodeAssistProvider,
        openai::OpenAiProvider,
        openai_responses::OpenAiResponsesProvider,
        wire::{ApiKey, WireRequest, WireResponse, WireTransport},
        CanonicalModelRequest, ModelContent, ModelEvent, ModelKey, ModelMessage, ModelProvider,
        ModelRole, ProviderError,
    },
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Records the body it was sent and answers with one canned SSE stream.
struct Recording {
    sent: Arc<Mutex<Vec<String>>>,
    stream: &'static str,
}

impl WireTransport for Recording {
    fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
        self.sent.lock().unwrap().push(request.body);
        Ok(WireResponse {
            status: 200,
            headers: Vec::new(),
            lines: Box::new(self.stream.lines().map(|line| Ok(line.to_owned()))),
        })
    }
}

/// Every provider this build speaks, each answering with a call to `fs.write`
/// in its own wire spelling.
fn providers(sent: &Arc<Mutex<Vec<String>>>) -> Vec<(&'static str, Box<dyn ModelProvider>)> {
    let transport = |stream: &'static str| Recording {
        sent: Arc::clone(sent),
        stream,
    };
    vec![
        (
            "anthropic",
            Box::new(AnthropicProvider::with_base_url(
                "https://anthropic.test",
                ApiKey::new("test"),
                transport(concat!(
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"fs_write\"}}\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"kept.txt\\\",\\\"content\\\":\\\"after\\\"}\"}}\n",
                    "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n",
                    "data: {\"type\":\"message_stop\"}\n",
                )),
            )),
        ),
        (
            "openai",
            Box::new(OpenAiProvider::with_base_url(
                "https://gateway.test/v1",
                ApiKey::new("test"),
                transport(concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"fs.write\",\"arguments\":\"{\\\"path\\\":\\\"kept.txt\\\",\\\"content\\\":\\\"after\\\"}\"}}]}}]}\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
                    "data: [DONE]\n",
                )),
            )),
        ),
        (
            "openai-responses",
            Box::new(OpenAiResponsesProvider::with_base_url(
                "https://codex.test",
                ApiKey::new("test"),
                transport(concat!(
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"fs_write\"}}\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"fs_write\",\"arguments\":\"{\\\"path\\\":\\\"kept.txt\\\",\\\"content\\\":\\\"after\\\"}\"}}\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
                )),
            )),
        ),
        (
            "google-code-assist",
            Box::new(
                GoogleCodeAssistProvider::with_base_url(
                    "https://gemini.test",
                    ApiKey::new("test"),
                    transport(
                        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"fs_write\",\"args\":{\"path\":\"kept.txt\",\"content\":\"after\"},\"id\":\"c1\"}}]},\"finishReason\":\"OTHER\"}]}}\n",
                    ),
                )
                .with_project("p"),
            ),
        ),
    ]
}

fn runtime(root: &std::path::Path) -> ToolRuntime {
    // Policy allows everything, so the only thing standing between the model
    // and a write is the execution mode.
    let rules = RuleSet::compile(CapabilityAction::ALL.iter().map(|action| PolicyRule {
        source: PolicySource::User,
        effect: RuleEffect::Allow,
        actor: ActorMatch::Any,
        action: *action,
        pattern: ResourcePattern::new(action.default_scheme(), "**").unwrap(),
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    }));
    let workspace = Workspace::open(root).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FileArtifactStore::open(root.join(".arsy/artifacts"), 0).unwrap());
    agent::runtime(
        &workspace,
        rules,
        artifacts,
        0,
        Principal::System,
        RiskContext {
            reversible: true,
            workspace: WorkspaceCleanliness::Clean,
            sandbox: SandboxAssurance::None,
        },
        arsy_code::operations::Reachable::default(),
        "test",
        arsy_code::operations::TurnState::default(),
        &[],
    )
    .unwrap()
}

/// Every tool name the request body offers, in whatever shape the dialect
/// nests its tool list: any `name` under a `tools` key.
fn offered(body: &Value) -> Vec<String> {
    fn names(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(name)) = map.get("name") {
                    out.push(name.clone());
                }
                map.values().for_each(|value| names(value, out));
            }
            Value::Array(items) => items.iter().for_each(|value| names(value, out)),
            _ => {}
        }
    }
    fn find(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "tools" {
                        names(value, out);
                    } else {
                        find(value, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|value| find(value, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    find(body, &mut out);
    out
}

#[test]
fn plan_mode_holds_for_every_provider_dialect() {
    const MUTATING: &[&str] = &[
        "fs.write",
        "fs.edit",
        "fs.delete",
        "fs.move",
        "apply_patch",
        "code.rename",
        "bash",
        "bash_start",
        "bash_write",
    ];
    let root = tempfile::tempdir().unwrap();
    let kept = root.path().join("kept.txt");
    std::fs::write(&kept, "before\n").unwrap();
    let plan = runtime(root.path()).with_execution_mode(ExecutionMode::Plan);
    let request = CanonicalModelRequest {
        model: ModelKey {
            provider: "any".to_owned(),
            model: "any".to_owned(),
        },
        system: None,
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "overwrite kept.txt".to_owned(),
            }],
        }],
        tools: plan.schemas(),
        max_output_tokens: 256,
        effort: None,
        idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
    };
    let sent = Arc::new(Mutex::new(Vec::new()));

    for (dialect, provider) in providers(&sent) {
        let events: Vec<ModelEvent> = provider
            .stream(&request)
            .unwrap_or_else(|error| panic!("{dialect}: {error:?}"))
            .map(|event| event.unwrap_or_else(|error| panic!("{dialect}: {error:?}")))
            .collect();

        // What reached the wire: reads, and no tool that could change anything,
        // in the canonical or the sanitized spelling.
        let body: Value = serde_json::from_str(sent.lock().unwrap().last().unwrap()).unwrap();
        let wire = offered(&body);
        assert!(
            wire.iter()
                .any(|name| name == "fs.read" || name == "fs_read"),
            "{dialect} offered no read tool: {wire:?}"
        );
        for name in MUTATING {
            let sanitized = name.replace('.', "_");
            assert!(
                !wire
                    .iter()
                    .any(|offered| offered == name || *offered == sanitized),
                "{dialect} put {name} on the wire in Plan mode: {wire:?}"
            );
        }

        // The model asked for the write anyway. Whatever name its adapter
        // decoded it to, the runtime refuses it.
        let (name, arguments) = events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ToolCallCompleted {
                    name, arguments, ..
                } => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{dialect} decoded no tool call: {events:?}"));
        if let Ok(prepared) = plan.prepare(&name, &arguments) {
            assert!(
                matches!(plan.authorize(&prepared), Authorization::Denied(_)),
                "{dialect}: {name} was authorized in Plan mode"
            );
        }
        let result = plan.invoke(&name, &arguments);
        assert!(!result.success, "{dialect}: {name} ran in Plan mode");
        // And the canonical spelling is refused the same way, so a dialect
        // that maps names back cannot reach a different outcome.
        let canonical = plan.invoke("fs.write", &arguments);
        assert!(
            canonical.output.contains("Blocked in Plan Mode"),
            "{dialect}: {}",
            canonical.output
        );
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "before\n",
            "{dialect} changed the workspace"
        );
    }
    assert_eq!(sent.lock().unwrap().len(), 4, "one request per dialect");
}
