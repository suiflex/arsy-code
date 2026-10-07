//! One scripted turn, end to end, against a provider on loopback.
//!
//! This is the only test that exercises the whole spine at once —
//! configuration, credential, routing, provider dialect, the tool runtime,
//! policy, the event store, and telemetry — so a change that breaks the seam
//! between two of them fails here rather than in production. Everything is
//! local: the "provider" is a socket this test owns, and the model's turn is a
//! script, so there is nothing to be flaky about.

use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

/// A fake OpenAI-dialect endpoint that replays one scripted response per
/// request and hands back the request bodies it was sent.
struct FakeProvider {
    port: u16,
    bodies: mpsc::Receiver<String>,
    /// How many subagent requests were open at once. Never above zero
    /// unless the provider was built by [`FakeProvider::delegating`].
    peak_children: std::sync::Arc<Barrier>,
}

/// Holds each arrival until `target` of them are waiting, or the wait gives
/// up, and remembers the most that were ever waiting together.
///
/// `std::sync::Barrier` cannot be used: it has no timeout, so a runtime that
/// really did serialize its subagents would hang the suite instead of
/// failing it with a number.
struct Barrier {
    target: usize,
    state: std::sync::Mutex<Waiting>,
    arrived: std::sync::Condvar,
}

#[derive(Default)]
struct Waiting {
    now: usize,
    peak: usize,
    /// Whether `target` were ever waiting together. Once they have been, the
    /// question is answered and nothing waits again — the later requests of
    /// a conversation are sequential by nature and would only pay the
    /// timeout.
    met: bool,
}

impl Barrier {
    fn new(target: usize) -> Self {
        Self {
            target,
            state: std::sync::Mutex::new(Waiting::default()),
            arrived: std::sync::Condvar::new(),
        }
    }

    fn arrive(&self) {
        let mut state = self.state.lock().unwrap();
        state.now += 1;
        state.peak = state.peak.max(state.now);
        state.met |= state.now >= self.target;
        self.arrived.notify_all();
        let deadline = std::time::Instant::now() + BARRIER_TIMEOUT;
        while !state.met {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            let (next, timed_out) = self.arrived.wait_timeout(state, remaining).unwrap();
            state = next;
            if timed_out.timed_out() {
                break;
            }
        }
        state.now -= 1;
    }

    fn peak(&self) -> usize {
        self.state.lock().unwrap().peak
    }
}

/// How long one subagent request waits for another to join it. Long enough
/// that a slow runner starting threads is not mistaken for a runtime that
/// runs its children one at a time.
const BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl FakeProvider {
    /// `script` holds one SSE body per expected request, in order.
    fn serving(script: Vec<String>) -> Self {
        Self::scripted(script.into_iter().map(Some).collect())
    }

    /// A `None` entry accepts the request and never answers, which is what a
    /// client sees when the process holding the turn is killed.
    fn scripted(script: Vec<Option<String>>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, bodies) = mpsc::channel();
        thread::spawn(move || {
            let mut held = Vec::new();
            for body in script {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut stream = stream;
                let request = read_request(&mut stream);
                // A receiver that has gone away means the test finished early;
                // the response still goes out so the client is not left hanging.
                let _ = sender.send(request);
                let Some(body) = body else {
                    // Hold the connection open without answering, and keep
                    // accepting: the test kills the client, then connects
                    // again to resume.
                    held.push(stream);
                    continue;
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            port,
            bodies,
            // Nothing arrives at this one, so its peak stays zero.
            peak_children: std::sync::Arc::new(Barrier::new(1)),
        }
    }

    /// Scripts served concurrently: one for the parent, and one per subagent.
    ///
    /// `children` is one script per subagent, keyed by a phrase that appears
    /// in that subagent's own request — its goal.
    ///
    /// Keyed rather than a single queue because subagents run at once, so
    /// which of them reaches the socket first is a race. One shared queue
    /// hands whichever child got there first the reply meant for another,
    /// and the test then measures the race instead of the runtime.
    fn delegating(parent: Vec<String>, children: Vec<(&str, Vec<String>)>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, bodies) = mpsc::channel();
        // With one subagent there is nothing to overlap with, so it must not
        // wait for a second that is never coming.
        let expect_concurrent = children.len().min(2);
        // Every queue remembers the reply it last served: a dropped
        // connection reads to the client as the peer disconnecting, which
        // `stream_with_retry` retries, and the retry must get the reply the
        // request before it got rather than the entry after it.
        let scripts = std::sync::Arc::new(std::sync::Mutex::new(Scripts {
            parent: Script::new(parent),
            children: children
                .into_iter()
                .map(|(marker, script)| (marker.to_owned(), Script::new(script)))
                .collect(),
        }));
        // Subagent requests are held until two of them are open at once, or
        // until the wait gives up.
        //
        // A fixed delay measured whether two happened to overlap inside it,
        // which is a question about how fast a runner starts threads —
        // Windows took longer than the delay and the probe saw nothing. A
        // barrier asks the question the test is about instead: can two be in
        // flight at the same time. A runtime that answered each child before
        // the next one asked would wait out the timeout and still show one.
        let gate = std::sync::Arc::new(Barrier::new(expect_concurrent));
        let barrier = std::sync::Arc::clone(&gate);
        thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let request = read_request(&mut stream);
                let _ = sender.send(request.clone());
                let child = is_child_request(&request);
                let mut held = scripts.lock().unwrap();
                let Some(body) = held.next_reply(child, &request) else {
                    continue;
                };
                drop(held);
                let body = fill_task_ids(&body, &request);
                let gate = std::sync::Arc::clone(&gate);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                thread::spawn(move || {
                    if !child {
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        return;
                    }
                    gate.arrive();
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        Self {
            port,
            bodies,
            peak_children: barrier,
        }
    }

    /// The most subagent requests that were in flight at the same moment.
    fn peak_children(&self) -> usize {
        self.peak_children.peak()
    }

    fn request(&self) -> Value {
        serde_json::from_str(&self.bodies.recv().expect("the provider was called")).unwrap()
    }

    /// Every request the provider was sent, once no more are coming.
    fn requests(&self) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Ok(body) = self
            .bodies
            .recv_timeout(std::time::Duration::from_millis(500))
        {
            seen.push(serde_json::from_str(&body).unwrap());
        }
        seen
    }
}

/// One correspondent's replies, in order, and the last one served.
struct Script {
    remaining: std::collections::VecDeque<String>,
    last: Option<String>,
}

impl Script {
    fn new(replies: Vec<String>) -> Self {
        Self {
            remaining: replies.into_iter().collect(),
            last: None,
        }
    }

    fn next(&mut self) -> Option<String> {
        match self.remaining.pop_front() {
            Some(reply) => {
                self.last = Some(reply.clone());
                Some(reply)
            }
            // A retry of the request this already answered.
            None => self.last.clone(),
        }
    }
}

/// The parent's script, and one per subagent keyed by its goal.
struct Scripts {
    parent: Script,
    children: Vec<(String, Script)>,
}

impl Scripts {
    fn next_reply(&mut self, child: bool, request: &str) -> Option<String> {
        if !child {
            return self.parent.next();
        }
        self.children
            .iter_mut()
            .find(|(marker, _)| request.contains(marker.as_str()))
            .and_then(|(_, script)| script.next())
    }
}

/// Replace `{task0}`, `{task1}`, … in a scripted reply with the task ids the
/// request has already reported.
///
/// A spawn's id is minted at run time, so a script cannot name it in advance.
/// Reading it back out of the conversation is what a real model would do.
fn fill_task_ids(body: &str, request: &str) -> String {
    if !body.contains("{task") {
        return body.to_owned();
    }
    let mut ids: Vec<String> = Vec::new();
    for marker in ["\\\"task\\\":\\\"", "\"task\":\""] {
        let mut rest = request;
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let id: String = rest.chars().take(36).collect();
            if looks_like_uuid(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut body = body.to_owned();
    for (index, id) in ids.iter().enumerate() {
        body = body.replace(&format!("{{task{index}}}"), id);
    }
    body
}

fn looks_like_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(at, character)| match at {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

/// Whether a request came from a subagent rather than the parent turn.
///
/// Read from the instructions the supervisor writes, which is the one thing
/// only a child's request carries.
fn is_child_request(body: &str) -> bool {
    body.contains("You are a subagent")
}

/// Read one HTTP request and return its body.
/// A `PreToolUse` guard on `fs.read` whose hook denies the call with `reason`.
///
/// The denial is written to a file in `directory` and the hook prints it, so
/// the command carries no quotes for a shell to disagree about.
fn denying_guard(directory: &Path, reason: &str) -> String {
    let answer = directory.join("deny.json");
    std::fs::write(
        &answer,
        format!(r#"{{"decision": "deny", "reason": "{reason}"}}"#),
    )
    .unwrap();
    // Printing the file rather than the JSON keeps every quote out of the
    // command line. `cmd /C` and `sh -c` disagree about quoting in ways no
    // single string satisfies: sh strips the double quotes the JSON needs, and
    // cmd strips them too once they reach it through Rust's own argument
    // quoting.
    let command = if cfg!(windows) {
        format!("type {}", answer.display())
    } else {
        format!("cat {}", answer.display())
    };
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "fs.read",
                "hooks": [{"type": "command", "command": command}]
            }]
        }
    })
    .to_string()
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

/// One SSE body from its `data:` payloads.
fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

fn asks_to_read(path: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-1",
            "type": "function",
            "function": {"name": "fs.read", "arguments": format!("{{\"path\": \"{path}\"}}")}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        serde_json::json!({"usage": {"prompt_tokens": 40, "completion_tokens": 8}}),
    ])
}

fn answers(text: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"content": text}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        serde_json::json!({"usage": {"prompt_tokens": 60, "completion_tokens": 12}}),
    ])
}

/// Write a settings file, given as TOML here and converted: the schema reads
/// more clearly that way than as quoted JSON, and what lands on disk is the
/// `arsy.json` the binary under test loads.
fn write_settings(path: &Path, body: &str) {
    let json = arsy_kernel::config::json_from_toml(body, path).unwrap();
    std::fs::write(path, json).unwrap();
}

fn settings_path(home: &Path) -> std::path::PathBuf {
    home.join(arsy_kernel::config::CONFIG_FILE)
}

fn configure(home: &Path, port: u16) {
    write_settings(
        &settings_path(home),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"test-model\"\n\
             context_windows = {{ test-model = 128000 }}\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             [policy]\n\
             default_effect = \"allow\"\n"
        ),
    );
}

fn arsy(workspace: &Path, home: &Path, args: &[&str]) -> (i32, Vec<Value>) {
    arsy_with_home(workspace, home, home, args)
}

/// The same run with the operator's home under the test's control, so the
/// hooks the engine finds are the ones the test wrote.
fn arsy_with_home(
    workspace: &Path,
    config_home: &Path,
    home: &Path,
    args: &[&str],
) -> (i32, Vec<Value>) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        // Never the Claude Code or Codex setup of the machine running the test.
        .env("CLAUDE_CONFIG_DIR", workspace.join("no-claude-home"))
        .env("CODEX_HOME", workspace.join("no-codex-home"))
        .args(args)
        .args(["--output", "json"])
        .env("ARSY_CONFIG_HOME", config_home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // Long enough that the redactor accepts it: a value short enough to
        // appear in ordinary text cannot be redacted safely and is refused.
        .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let records = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    (output.status.code().unwrap_or(-1), records)
}

fn result(records: &[Value]) -> &Value {
    records
        .iter()
        .find(|record| record["type"] == "result")
        .map(|record| &record["payload"])
        .unwrap_or_else(|| panic!("no result record in {records:#?}"))
}

/// `model.effort` is what a scripted run asks for, clamped to what the model
/// takes; unset, the request carries no effort, as it always did.
#[test]
fn a_run_sends_the_configured_effort_and_none_without_it() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let configured = |port: u16, effort: &str| {
        write_settings(
            &settings_path(home.path()),
            &format!(
                "schema_version = 1\n\
                 [provider.endpoint.local]\n\
                 kind = \"openai\"\n\
                 base_url = \"http://127.0.0.1:{port}\"\n\
                 model = \"test-model\"\n\
                 context_windows = {{ test-model = 128000 }}\n\
                 efforts = {{ test-model = [\"low\", \"medium\"] }}\n\
                 api_key_env = \"ARSY_TEST_KEY\"\n\
                 [policy]\n\
                 default_effect = \"allow\"\n\
                 {effort}"
            ),
        );
    };

    let provider = FakeProvider::serving(vec![answers("ok.")]);
    configured(provider.port, "[model]\neffort = \"high\"\n");
    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ok"]);
    assert_eq!(code, 0, "{records:#?}");
    // High is past what the model takes, so it is clamped to its highest.
    assert_eq!(provider.request()["reasoning_effort"], "medium");

    let provider = FakeProvider::serving(vec![answers("ok.")]);
    configured(provider.port, "");
    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ok"]);
    assert_eq!(code, 0, "{records:#?}");
    assert!(provider.request().get("reasoning_effort").is_none());
}

/// Unattended, a model that keeps retrying a call that fails is warned at
/// the second failure and stopped at the third, instead of spending the
/// whole round budget on it.
#[test]
fn a_run_stuck_on_one_failing_call_is_warned_then_stopped() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![
        asks_to_read("missing.txt"),
        asks_to_read("missing.txt"),
        asks_to_read("missing.txt"),
        asks_to_read("missing.txt"),
    ]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "read missing.txt"]);

    assert_ne!(code, 0, "{records:#?}");
    let output = serde_json::to_string(&records).unwrap();
    assert!(output.contains("same failing tool call"), "{output}");
    let _first = provider.request();
    let _second = provider.request();
    let third = provider.request().to_string();
    assert!(
        third.contains("failed twice"),
        "the warning came first: {third}"
    );
    assert!(
        provider
            .bodies
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err(),
        "no fourth request after the stop"
    );
}

#[test]
fn a_scripted_turn_reads_a_file_answers_and_reports_what_it_spent() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["provider"], "local");

    // The first request carried the task and the tool schemas; the second
    // carried the file the tool actually read, which is the whole point of the
    // loop — the model's next request sees the effect of its last call.
    let first = provider.request();
    assert_eq!(first["model"], "test-model");
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"fs.read"), "{tools:?}");
    let second = provider.request().to_string();
    assert!(second.contains("the answer is 42"), "{second}");

    let telemetry = &result["telemetry"];
    assert_eq!(telemetry["stop"], "answered");
    assert_eq!(telemetry["model_calls"], 2);
    assert_eq!(telemetry["tool_calls"], 1);
    assert_eq!(telemetry["tool_failures"], 0);
    assert_eq!(telemetry["input_tokens"], 100);
    assert_eq!(telemetry["output_tokens"], 20);
    assert_eq!(telemetry["retries"], 0);
    assert_eq!(telemetry["dropped"], 0);
    // Nothing was configured to export to, so nothing left the machine.
    assert_eq!(telemetry["exported"], 0);

    // What the turn spent is in the store, so `arsy session show` answers
    // "what did that run cost" after the process has exited.
    let session = result["session"].as_str().unwrap();
    let (code, shown) = arsy(
        workspace.path(),
        home.path(),
        &["session", "show", session, "--turns"],
    );
    assert_eq!(code, 0);
    let shown = self::result(&shown);
    assert_eq!(shown["turns"], 1);
    assert_eq!(shown["turn_detail"][0]["status"], "completed");
    assert_eq!(shown["usage"]["input_tokens"], 100);
    assert_eq!(shown["usage"]["output_tokens"], 20);
}

#[test]
fn a_run_discovers_the_selected_models_window_without_a_configured_number() {
    for (field, limit) in [
        ("context_window", 128_000),
        ("context_window", 1_000_000),
        ("max_input_tokens", 1_000_000),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut metadata = serde_json::json!({"data": [{"id": "test-model"}]});
        metadata["data"][0][field] = limit.into();
        metadata["data"][0]["max_output_tokens"] = 16_384.into();
        let provider = FakeProvider::serving(vec![metadata.to_string(), answers("ready")]);
        configure(home.path(), provider.port);
        let path = settings_path(home.path());
        let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        settings["provider"]["endpoint"]["local"]
            .as_object_mut()
            .unwrap()
            .remove("context_windows");
        std::fs::write(path, settings.to_string()).unwrap();

        let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
        assert_eq!(code, 0, "{records:#?}");
        assert_eq!(result(&records)["status"], "completed");
        assert_eq!(provider.bodies.recv().unwrap(), ""); // metadata GET
        let request = provider.request();
        assert_eq!(request["model"], "test-model");
        assert_eq!(request["max_tokens"], 16_384);
        let saved: Value =
            serde_json::from_slice(&std::fs::read(settings_path(home.path())).unwrap()).unwrap();
        assert!(saved["provider"]["endpoint"]["local"]
            .get("context_windows")
            .is_none());
    }
}

#[test]
fn a_run_uses_exact_input_and_output_limits_without_discovery() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![answers("ready")]);
    configure(home.path(), provider.port);
    let path = settings_path(home.path());
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let endpoint = settings["provider"]["endpoint"]["local"]
        .as_object_mut()
        .unwrap();
    endpoint.remove("context_windows");
    endpoint.insert("input_limits".to_owned(), json!({"test-model": 64_000}));
    endpoint.insert("output_limits".to_owned(), json!({"test-model": 16_384}));
    std::fs::write(path, settings.to_string()).unwrap();

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_eq!(code, 0, "{records:#?}");
    let request = provider.request();
    assert_eq!(request["model"], "test-model");
    assert_eq!(request["max_tokens"], 16_384);
}

#[test]
fn a_run_refuses_a_model_whose_provider_reports_no_limit() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let metadata = serde_json::json!({"data": [{"id": "test-model"}]});
    let provider = FakeProvider::serving(vec![metadata.to_string()]);
    configure(home.path(), provider.port);
    let path = settings_path(home.path());
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["provider"]["endpoint"]["local"]
        .as_object_mut()
        .unwrap()
        .remove("context_windows");
    std::fs::write(path, settings.to_string()).unwrap();

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_ne!(code, 0);
    assert!(serde_json::to_string(&records)
        .unwrap()
        .contains("did not report a context limit"));
    assert!(serde_json::to_string(&records)
        .unwrap()
        .contains("provider.endpoint.local.context_windows.test-model"));
    assert_eq!(provider.bodies.recv().unwrap(), "");
}

#[test]
fn antigravity_discovery_without_a_limit_uses_the_documented_flash_window() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![
        r#"{"models":{"gemini-3.8-flash-medium":{}}}"#.to_owned(),
        r#"{"cloudaicompanionProject":"test-project"}"#.to_owned(),
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ready\"}]},\"finishReason\":\"STOP\"}]}}\n".to_owned(),
    ]);
    write_settings(
        &settings_path(home.path()),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.antigravity]\n\
             kind = \"google_code_assist\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"gemini-3.8-flash-medium\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n",
            provider.port
        ),
    );

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_eq!(code, 0, "{records:#?}");
    assert_eq!(result(&records)["status"], "completed");
    assert_eq!(provider.bodies.recv().unwrap(), "{}");
    assert_eq!(provider.request()["metadata"]["ideType"], "ANTIGRAVITY");
    let request = provider.request();
    assert_eq!(request["model"], "gemini-3.8-flash-medium");
    assert_eq!(
        request["request"]["generationConfig"]["maxOutputTokens"],
        65_536
    );
}

#[test]
fn antigravity_uses_its_reported_max_tokens_for_an_unknown_model() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![
        r#"{"models":{"other-model":{"maxTokens":200000,"maxOutputTokens":64000}}}"#.to_owned(),
        r#"{"cloudaicompanionProject":"test-project"}"#.to_owned(),
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ready\"}]},\"finishReason\":\"STOP\"}]}}\n".to_owned(),
    ]);
    write_settings(
        &settings_path(home.path()),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.antigravity]\n\
             kind = \"google_code_assist\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"other-model\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n",
            provider.port
        ),
    );

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_eq!(code, 0, "{records:#?}");
    assert_eq!(result(&records)["status"], "completed");
    assert_eq!(provider.bodies.recv().unwrap(), "{}");
    assert_eq!(provider.request()["metadata"]["ideType"], "ANTIGRAVITY");
    let request = provider.request();
    assert_eq!(request["model"], "other-model");
    assert_eq!(
        request["request"]["generationConfig"]["maxOutputTokens"],
        64_000
    );
}

#[test]
fn antigravity_still_refuses_an_unknown_model_without_a_limit() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![r#"{"models":{"other-model":{}}}"#.to_owned()]);
    write_settings(
        &settings_path(home.path()),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.antigravity]\n\
             kind = \"google_code_assist\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"other-model\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n",
            provider.port
        ),
    );

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_ne!(code, 0);
    assert!(serde_json::to_string(&records)
        .unwrap()
        .contains("provider.endpoint.antigravity.context_windows.other-model"));
    assert_eq!(provider.bodies.recv().unwrap(), "{}");
}

#[test]
fn an_exact_verified_antigravity_variant_override_avoids_limit_discovery() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![
        r#"{"cloudaicompanionProject":"test-project"}"#.to_owned(),
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ready\"}]},\"finishReason\":\"STOP\"}]}}\n".to_owned(),
    ]);
    write_settings(
        &settings_path(home.path()),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.antigravity]\n\
             kind = \"google_code_assist\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"gemini-3.8-flash-medium\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             context_windows = {{ \"gemini-3.8-flash-medium\" = 128000 }}\n\
             output_limits = {{ \"gemini-3.8-flash-medium\" = 32768 }}\n\
             [policy]\n\
             default_effect = \"allow\"\n",
            provider.port
        ),
    );

    // A configured exact ID skips metadata altogether; the fake provider
    // sees project discovery and the model call, but no model discovery.
    // The number is fixture data, not a claimed real Antigravity limit.
    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_eq!(code, 0, "{records:#?}");
    assert_eq!(result(&records)["status"], "completed");
    assert_eq!(provider.request()["metadata"]["ideType"], "ANTIGRAVITY");
    let request = provider.request();
    assert_eq!(request["model"], "gemini-3.8-flash-medium");
    assert_eq!(
        request["request"]["generationConfig"]["maxOutputTokens"],
        32_768
    );
    assert_eq!(
        request["request"]["contents"][0]["parts"][0]["text"],
        "say ready"
    );
}

#[test]
fn an_exact_antigravity_context_override_uses_the_documented_output_limit() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![
        r#"{"cloudaicompanionProject":"test-project"}"#.to_owned(),
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ready\"}]},\"finishReason\":\"STOP\"}]}}\n".to_owned(),
    ]);
    write_settings(
        &settings_path(home.path()),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.antigravity]\n\
             kind = \"google_code_assist\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"gemini-3.8-flash-medium\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             context_windows = {{ \"gemini-3.8-flash-medium\" = 128000 }}\n",
            provider.port
        ),
    );

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "say ready"]);
    assert_eq!(code, 0, "{records:#?}");
    assert_eq!(provider.request()["metadata"]["ideType"], "ANTIGRAVITY");
    let request = provider.request();
    assert_eq!(
        request["request"]["generationConfig"]["maxOutputTokens"],
        65_536
    );
}

#[test]
fn a_refused_tool_reaches_the_model_as_a_failed_result_and_is_counted() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // The file does not exist, so the call fails inside the runtime rather
    // than at the policy gate: the turn must continue and the model must be
    // told, because a provider that sent a call and never sees its result
    // rejects the next request.
    let provider = FakeProvider::serving(vec![asks_to_read("missing.txt"), answers("no file.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "read missing.txt"]);

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["telemetry"]["tool_calls"], 1);
    assert_eq!(result["telemetry"]["tool_failures"], 1);

    drop(provider.request());
    let second = provider.request();
    let messages = second["messages"].as_array().unwrap();
    let tool_result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the failed call was reported back");
    assert!(
        tool_result["content"]
            .as_str()
            .unwrap()
            .starts_with("error:"),
        "{tool_result}"
    );
}

#[test]
fn a_task_its_process_never_finished_is_continued_by_resume() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    // The first request is accepted and never answered, so the run is still
    // holding its task when the test kills it -- a process that died mid-turn.
    let provider = FakeProvider::scripted(vec![
        None,
        Some(asks_to_read("notes.txt")),
        Some(answers("42.")),
    ]);
    configure(home.path(), provider.port);

    let mut child = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.path().to_str().unwrap()])
        // Never the Claude Code or Codex setup of the machine running the test.
        .env("CLAUDE_CONFIG_DIR", workspace.path().join("no-claude-home"))
        .env("CODEX_HOME", workspace.path().join("no-codex-home"))
        .args(["run", "what does notes.txt say?"])
        .args(["--output", "json"])
        .env("ARSY_CONFIG_HOME", home.path())
        .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
        .stdout(Stdio::null())
        .spawn()
        .expect("the binary runs");
    // The provider has the request, so the turn and its task are on the record.
    drop(provider.request());
    child.kill().expect("the run is killed mid-turn");
    child.wait().expect("the killed run is reaped");

    let (code, listed) = arsy(workspace.path(), home.path(), &["session", "list"]);
    assert_eq!(code, 0);
    let listed = result(&listed);
    let session = listed["sessions"][0]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(listed["sessions"][0]["status"], "running");

    // Resume closes the open turn, takes the task back, and finishes it with
    // the goal it was created with -- which nothing but the store still knows.
    let (code, records) = arsy(workspace.path(), home.path(), &["resume", &session]);
    let resumed = result(&records);
    assert_eq!(code, 0, "{resumed:#?}");
    assert_eq!(resumed["closed_turns"], 1);
    assert_eq!(resumed["recovered_tasks"], 1);
    assert_eq!(resumed["continuing"], resumed["task"]);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(resumed["telemetry"]["tool_calls"], 1);

    // The resumed turn asked the same question the killed one did.
    let continued = provider.request().to_string();
    assert!(
        continued.contains("what does notes.txt say?"),
        "{continued}"
    );

    // Nothing is left waiting once it completed.
    let (code, records) = arsy(workspace.path(), home.path(), &["resume", &session]);
    assert_eq!(code, 0);
    let quiet = result(&records);
    assert_eq!(quiet["continuing"], Value::Null);
    assert_eq!(quiet["recovered_tasks"], 0);
}

#[test]
fn what_the_workspace_remembers_reaches_the_model_and_can_be_withdrawn() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![answers("noted."), answers("noted again.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "remember",
            "the test command is `cargo nextest run`",
        ],
    );
    assert_eq!(code, 0, "{records:#?}");
    let memory = result(&records)["memory"].as_str().unwrap().to_owned();

    let (code, records) = arsy(workspace.path(), home.path(), &["memory", "list"]);
    assert_eq!(code, 0);
    let listed = result(&records);
    assert_eq!(listed["memories"].as_array().unwrap().len(), 1);
    assert_eq!(listed["memories"][0]["scope"], "repository");
    assert_eq!(
        listed["memories"][0]["claim"],
        "the test command is `cargo nextest run`"
    );
    // Typed at a prompt with nothing checking it: reported, whatever authority
    // the operator has.
    assert_eq!(listed["memories"][0]["confidence"], "reported");

    let (code, _) = arsy(workspace.path(), home.path(), &["run", "how do I test?"]);
    assert_eq!(code, 0);
    let asked = provider.request().to_string();
    assert!(asked.contains("cargo nextest run"), "{asked}");

    // Withdrawn, and the next turn is not told it.
    let (code, _) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "forget",
            &memory,
            "--to",
            "we moved back to cargo test",
        ],
    );
    assert_eq!(code, 0);
    let (code, _) = arsy(workspace.path(), home.path(), &["run", "how do I test?"]);
    assert_eq!(code, 0);
    let asked = provider.request().to_string();
    assert!(!asked.contains("cargo nextest run"), "{asked}");

    // The tombstone stays, with the reason.
    let (_, records) = arsy(workspace.path(), home.path(), &["memory", "list", "--all"]);
    let all = result(&records);
    assert_eq!(all["memories"][0]["status"], "revoked");
    assert_eq!(
        all["memories"][0]["revocation"],
        "we moved back to cargo test"
    );
}

/// A reply that asks for one subagent and stops.
fn spawns(goal: &str, capabilities: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-spawn",
            "type": "function",
            "function": {
                "name": "task.spawn",
                "arguments": format!("{{\"goal\": \"{goal}\", \"capabilities\": [{capabilities}]}}")
            }
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A parent reply that waits for every subagent it started.
fn waits() -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-wait",
            "type": "function",
            "function": {"name": "task.wait", "arguments": "{}"}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A child reply that asks to write, which its own grants must refuse.
fn tries_to_write() -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-write",
            "type": "function",
            "function": {"name": "fs.write", "arguments": "{\"path\": \"escaped.txt\", \"content\": \"x\"}"}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

#[test]
fn a_subagent_holds_less_authority_than_the_parent_that_spawned_it() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    // The parent starts a subagent, keeps its turn, and only then waits for
    // the answer. The child reads, tries to write, and answers — on its own
    // thread, against its own half of the script.
    let provider = FakeProvider::delegating(
        vec![
            spawns("what does notes.txt say", "\"fs.read\""),
            waits(),
            answers("the subagent found 42."),
        ],
        vec![(
            "what does notes.txt say",
            vec![
                asks_to_read("notes.txt"),
                tries_to_write(),
                answers("notes.txt says 42."),
            ],
        )],
    );
    configure_delegating(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &["run", "find out what notes.txt says"],
    );
    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["status"], "completed");

    let seen = provider.requests();
    let (child, parent): (Vec<&Value>, Vec<&Value>) = seen
        .iter()
        .partition(|body| is_child_request(&body.to_string()));

    // The parent was offered the whole supervisor surface, because this policy
    // delegates. Waiting is a separate call now, which is what makes starting
    // one child and continuing to work possible at all.
    let tools: Vec<&str> = parent[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"task.spawn"), "{tools:?}");
    assert!(tools.contains(&"task.wait"), "{tools:?}");

    // What `task.spawn` returned: ids and nothing to wait on, rather than the
    // child's answer.
    let spawned = last_tool_result(parent[1]);
    assert!(
        !spawned.starts_with("error:"),
        "the spawn failed: {spawned}"
    );
    let spawned: Value = serde_json::from_str(&spawned).unwrap();
    assert!(spawned["task"].is_string(), "{spawned}");
    assert!(spawned["attempt"].is_string(), "{spawned}");

    // The child holds only what was delegated: its write was refused by its
    // own runtime, not by the parent's.
    let refused = last_tool_result(child.last().expect("the child asked something"));
    assert!(refused.starts_with("error:"), "{refused}");
    assert!(
        !workspace.path().join("escaped.txt").exists(),
        "a subagent must not be able to write"
    );

    // And `task.wait` is where the answer reaches the parent.
    let waited = last_tool_result(parent[2]);
    assert!(waited.contains("notes.txt says 42."), "{waited}");
}

/// A workspace whose policy lets the parent write and lets it delegate reads.
fn configure_delegating(home: &Path, port: u16) {
    write_settings(
        &settings_path(home),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"test-model\"\n\
             context_windows = {{ test-model = 128000 }}\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             # Delegation is off unless a rule says otherwise, so the depth is\n\
             # what makes a subagent possible at all.\n\
             [[policy.rules]]\n\
             id = \"delegate-reads\"\n\
             effect = \"allow\"\n\
             action = \"fs.read\"\n\
             resource = \"file:**\"\n\
             delegation_depth = 2\n\
             [[policy.rules]]\n\
             id = \"parent-writes\"\n\
             effect = \"allow\"\n\
             action = \"fs.write\"\n\
             resource = \"file:**\"\n"
        ),
    );
}

/// Three `task.spawn` calls in one reply, so the parent starts them all
/// before it reads any of them back.
fn spawns_three() -> String {
    let call = |index: usize, goal: &str| {
        serde_json::json!({
            "index": index,
            "id": format!("call-spawn-{index}"),
            "type": "function",
            "function": {
                "name": "task.spawn",
                "arguments": format!("{{\"goal\": \"{goal}\", \"capabilities\": [\"fs.read\"]}}")
            }
        })
    };
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [
            call(0, "what does one.txt say"),
            call(1, "what does two.txt say"),
            call(2, "what does three.txt say"),
        ]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

#[test]
fn three_subagents_investigate_at_once_while_the_parent_keeps_its_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // The parent starts three children in one reply, asks what they are doing
    // while they work, and only then waits. Each child answers in one round.
    let provider = FakeProvider::delegating(
        vec![
            spawns_three(),
            status(),
            waits(),
            answers("all three reported."),
        ],
        vec![
            ("what does one.txt say", vec![answers("one says A.")]),
            ("what does two.txt say", vec![answers("two says B.")]),
            ("what does three.txt say", vec![answers("three says C.")]),
        ],
    );
    configure_delegating(home.path(), provider.port);

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "survey the repo"]);
    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");

    let seen = provider.requests();
    let parent: Vec<&Value> = seen
        .iter()
        .filter(|body| !is_child_request(&body.to_string()))
        .collect();

    // The claim of the phase: the children overlapped rather than taking
    // turns, and the parent was still issuing calls while they did.
    let children = seen.len() - parent.len();
    assert!(
        provider.peak_children() > 1,
        "subagents ran one after another, not at once: {children} child requests, \
         parent asked {} times, spawn said {}",
        parent.len(),
        last_tool_result(parent[1])
    );
    let reported = last_tool_result(parent[2]);
    assert!(reported.contains("\"state\":\"running\""), "{reported}");

    // And every answer reaches the parent through one wait.
    let waited = last_tool_result(parent[3]);
    for answer in ["one says A.", "two says B.", "three says C."] {
        assert!(waited.contains(answer), "{waited}");
    }
}

/// Two isolated writers in one reply, each on its own component.
fn spawns_two_writers() -> String {
    let call = |index: usize, goal: &str| {
        serde_json::json!({
            "index": index,
            "id": format!("call-writer-{index}"),
            "type": "function",
            "function": {
                "name": "task.spawn",
                "arguments": serde_json::json!({
                    "goal": goal,
                    "capabilities": ["fs.read", "fs.write"],
                    "workspace": "isolated_writer",
                }).to_string()
            }
        })
    };
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [
            call(0, "rewrite one.txt"),
            call(1, "rewrite two.txt"),
        ]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A writer's reply: change one file, then report the manifest its task asked
/// for. `fs.write` is the child's own grant, into its own view.
fn writes(path: &str, content: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": format!("call-write-{path}"),
            "type": "function",
            "function": {
                "name": "fs.write",
                "arguments": serde_json::json!({"path": path, "content": content}).to_string()
            }
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A writer's manifest, as the object its `required_output` demands.
fn reports(path: &str) -> String {
    answers(
        &serde_json::json!({
            "summary": format!("rewrote {path}"),
            "changed_files": [path],
            "validation": ["artifact-check"],
            "unresolved": [],
        })
        .to_string(),
    )
}

/// A parent reply that applies one writer's work.
fn integrates(index: usize) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": format!("call-integrate-{index}"),
            "type": "function",
            "function": {
                "name": "task.integrate",
                // The id is not known when the script is written, so the
                // arguments are filled in from the spawn result by the fake
                // provider's caller. `{task}` is replaced below.
                "arguments": format!("{{\"task\": \"{{task{index}}}\"}}")
            }
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

#[test]
fn two_writers_change_separate_components_and_an_integrator_applies_both() {
    let Some(workspace) = git_workspace(&[("one.txt", "base one\n"), ("two.txt", "base two\n")])
    else {
        eprintln!("skipped: git is not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();

    // Two writers, each in a tree of its own, then two integrations into the
    // workspace the parent owns.
    let provider = FakeProvider::delegating(
        vec![
            spawns_two_writers(),
            waits(),
            integrates(0),
            integrates(1),
            answers("both changes are in."),
        ],
        vec![
            (
                "rewrite one.txt",
                vec![writes("one.txt", "from writer one\n"), reports("one.txt")],
            ),
            (
                "rewrite two.txt",
                vec![writes("two.txt", "from writer two\n"), reports("two.txt")],
            ),
        ],
    );
    configure_delegating_writers(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &["run", "split this work in two"],
    );
    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");

    let seen = provider.requests();
    let trail: Vec<String> = seen
        .iter()
        .filter(|body| !is_child_request(&body.to_string()))
        .map(last_tool_result)
        .collect();
    let child_trail: Vec<String> = seen
        .iter()
        .filter(|body| is_child_request(&body.to_string()))
        .map(last_tool_result)
        .collect();

    // Both writers' work reached the target, and neither overwrote the other.
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("one.txt")).unwrap(),
        "from writer one\n",
        "parent saw {trail:#?}\nchildren saw {child_trail:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("two.txt")).unwrap(),
        "from writer two\n"
    );

    // The parent's last two tool results are the two integrations.
    for applied in &trail[3..5] {
        assert!(applied.contains("\"applied\""), "{trail:#?}");
    }

    // The views the writers worked in are gone, and nothing they wrote
    // reached the workspace except through the integrator.
    let views = workspace.path().join(arsy_code::workspace::VIEWS);
    let remaining: Vec<_> = std::fs::read_dir(&views)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(
        remaining.is_empty(),
        "isolated views were left behind: {remaining:?}"
    );
}

/// A Git workspace with the given files committed, or `None` where Git is
/// not installed.
fn git_workspace(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
    let workspace = tempfile::tempdir().unwrap();
    let git = |arguments: &[&str]| {
        Command::new("git")
            .args(arguments)
            .current_dir(workspace.path())
            .output()
            .ok()
            .filter(|output| output.status.success())
    };
    git(&["init", "--quiet"])?;
    git(&["config", "user.name", "ARSY Test"])?;
    git(&["config", "user.email", "arsy@example.invalid"])?;
    // Windows runners rewrite line endings on checkout by default, and this
    // test compares a writer's file byte for byte to say whether its work
    // arrived. That question is not about Git's line-ending policy.
    git(&["config", "core.autocrlf", "false"])?;
    for (path, content) in files {
        std::fs::write(workspace.path().join(path), content).unwrap();
    }
    git(&["add", "--all"])?;
    git(&["commit", "--quiet", "-m", "base"])?;
    Some(workspace)
}

/// As `configure_delegating`, and writing is delegable too — which is what
/// makes an isolated writer possible.
fn configure_delegating_writers(home: &Path, port: u16) {
    write_settings(
        &settings_path(home),
        &format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"test-model\"\n\
             context_windows = {{ test-model = 128000 }}\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             [[policy.rules]]\n\
             id = \"delegate-reads\"\n\
             effect = \"allow\"\n\
             action = \"fs.read\"\n\
             resource = \"file:**\"\n\
             delegation_depth = 2\n\
             [[policy.rules]]\n\
             id = \"delegate-writes\"\n\
             effect = \"allow\"\n\
             action = \"fs.write\"\n\
             resource = \"file:**\"\n\
             delegation_depth = 2\n"
        ),
    );
}

/// A reply that commits one acceptance criterion settled by a command.
fn commits_criterion(statement: &str, check: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-criterion",
            "type": "function",
            "function": {
                "name": "task.criterion",
                "arguments": serde_json::json!({
                    "statement": statement,
                    "check": check,
                }).to_string()
            }
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

#[test]
fn a_turn_that_finishes_without_running_its_own_check_is_not_verified() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // The model commits to a criterion, then answers as if it were done —
    // which is exactly the claim proof-carrying completion refuses.
    let provider = FakeProvider::serving(vec![
        commits_criterion("the suite passes", "cargo test --workspace"),
        answers("all done, the tests pass."),
    ]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "fix the parser"]);
    let ran = result(&records);
    assert_eq!(code, 0, "{ran:#?}");
    // The turn itself completed: execution finished is a true statement.
    assert_eq!(ran["status"], "completed");

    // And the turn reported, in its own records, that nothing proves it.
    let proof = records
        .iter()
        .find(|record| record["type"] == "task.proof")
        .expect("the turn reports what its criteria say");
    assert_eq!(proof["payload"]["state"], "unverified");
    assert_eq!(
        proof["payload"]["outstanding"][0]["why"],
        "this check has not been run"
    );

    // `arsy verify`, run from outside, reaches the same verdict and says so
    // with a nonzero exit code — which is what a CI job reads.
    let session = ran["session"].as_str().expect("a session id");
    let (verify_code, verify_records) = arsy(workspace.path(), home.path(), &["verify", session]);
    assert_ne!(verify_code, 0, "{verify_records:#?}");
    let verdict = result(&verify_records);
    assert_eq!(verdict["state"], "unverified", "{verdict:#?}");
    assert_eq!(
        verdict["tasks"][0]["criteria"][0]["statement"],
        "the suite passes"
    );

    // A session that is not here is a question about the id, not a verdict
    // about the work: answering a typo with "unverified" reads as a finding.
    let (missing_code, missing_records) = arsy(
        workspace.path(),
        home.path(),
        &["verify", "00000000-0000-4000-8000-000000000000"],
    );
    assert_ne!(missing_code, 0);
    assert_eq!(result(&missing_records)["code"], "ARSY-SCH-1004");

    // And verifying is read-only: a workspace it was pointed at by mistake
    // is left exactly as it was found.
    let untouched = tempfile::tempdir().unwrap();
    let (fresh_code, _) = arsy(untouched.path(), home.path(), &["verify", session]);
    assert_ne!(fresh_code, 0);
    assert!(
        !untouched.path().join(".arsy").exists(),
        "verifying created state in a workspace it only read"
    );
}

/// A parent reply that asks what its subagents are doing.
fn status() -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-status",
            "type": "function",
            "function": {"name": "task.status", "arguments": "{}"}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// The content of the last tool result in a request's messages.
fn last_tool_result(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

#[test]
fn a_claim_that_looks_like_a_credential_is_never_written_to_the_artifact_store() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![answers("noted.")]);
    configure(home.path(), provider.port);

    let artifacts = workspace.path().join(arsy_code::workspace::ARTIFACTS);
    let count = || -> usize { walk(&artifacts).len() };

    // A claim that is fine is stored, so the comparison below is against a
    // store that is working rather than one that never writes.
    let (code, _) = arsy(
        workspace.path(),
        home.path(),
        &["memory", "remember", "the build uses cargo"],
    );
    assert_eq!(code, 0);
    let after_good = count();
    assert!(after_good > 0, "an accepted claim is stored");

    // A claim carrying something shaped like a key is refused -- and nothing
    // is left behind, because a memory claim is kept alive by retention rather
    // than by reachability, so an orphan here is uncollectable forever.
    let (code, refused) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "remember",
            "the deploy token is ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        ],
    );
    assert_eq!(code, 3, "{refused:#?}");
    assert_eq!(
        count(),
        after_good,
        "a refused claim wrote a blob that nothing can collect"
    );
}

/// Every file under a directory, if it exists.
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            found.extend(walk(&entry.path()));
        } else {
            found.push(entry.path());
        }
    }
    found
}

/// A hook that denies a tool call, on a real turn, from the operator's own
/// file — the whole path from a declaration on disk to a call that does not
/// happen.
#[test]
fn a_hook_denies_a_tool_call_and_the_model_is_told_why() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    // `arsy_with_home` points `ARSY_CONFIG_HOME` here, so this is the
    // operator's `~/.arsy/guard.json`.
    std::fs::write(
        home.path().join("guard.json"),
        denying_guard(home.path(), "notes are off limits"),
    )
    .unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");

    let _first = provider.request();
    // The second request carries what the tool call produced. The hook denied
    // it, so it carries the refusal and its reason — and not the file.
    let second = provider.request();
    let transcript = second.to_string();
    assert!(
        transcript.contains("notes are off limits"),
        "the model was told why: {transcript}"
    );
    assert!(
        !transcript.contains("the answer is 42"),
        "the file was never read: {transcript}"
    );
}

/// `ARSY_CONFIG_HOME` moves the operator's guard with the rest of their ARSY
/// files: a `guard.json` left in `$HOME/.arsy` is not the one in force.
#[test]
fn the_user_guard_follows_the_arsy_config_home() {
    let workspace = tempfile::tempdir().unwrap();
    let config_home = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    std::fs::create_dir_all(home.path().join(".arsy")).unwrap();
    std::fs::write(
        home.path().join(".arsy/guard.json"),
        denying_guard(&home.path().join(".arsy"), "the stale home said no"),
    )
    .unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(config_home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        config_home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let unguarded = provider.request().to_string();
    assert!(
        !unguarded.contains("the stale home said no"),
        "the guard outside the config home did not run: {unguarded}"
    );
    assert!(
        unguarded.contains("the answer is 42"),
        "so the read happened: {unguarded}"
    );

    // The same guard inside the configuration home is in force.
    std::fs::write(
        config_home.path().join("guard.json"),
        denying_guard(config_home.path(), "the config home said no"),
    )
    .unwrap();
    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure(config_home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        config_home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let guarded = provider.request().to_string();
    assert!(
        guarded.contains("the config home said no"),
        "the guard in the config home ran: {guarded}"
    );
    assert!(
        !guarded.contains("the answer is 42"),
        "the file was never read: {guarded}"
    );
}

/// A hook added with `arsy hook add` is one the engine runs, and `arsy hook
/// remove` takes it away again: the commands edit the file the engine reads.
#[test]
fn a_hook_added_from_the_cli_denies_and_its_removal_allows() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    let guard: Value =
        serde_json::from_str(&denying_guard(home.path(), "added from the cli")).unwrap();
    let command = guard["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &[
            "hook",
            "add",
            "--event",
            "PreToolUse",
            "--matcher",
            "fs.read",
            "--command",
            &command,
        ],
    );
    assert_eq!(code, 0, "{records:#?}");
    assert!(home.path().join("guard.json").is_file());

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure(home.path(), provider.port);
    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let denied = provider.request().to_string();
    assert!(denied.contains("added from the cli"), "{denied}");

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["hook", "remove", "PreToolUse", "0"],
    );
    assert_eq!(code, 0, "{records:#?}");

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(home.path(), provider.port);
    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let allowed = provider.request().to_string();
    assert!(allowed.contains("the answer is 42"), "{allowed}");
}

/// The same file in the repository rather than the operator's home does
/// nothing until the operator vouches for that directory.
#[test]
fn a_repositorys_own_hook_does_not_run_until_it_is_vouched_for() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    std::fs::create_dir_all(workspace.path().join(".arsy")).unwrap();
    std::fs::write(
        workspace.path().join(".arsy/guard.json"),
        denying_guard(&workspace.path().join(".arsy"), "the repo said no"),
    )
    .unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));

    let _first = provider.request();
    let unvouched = provider.request().to_string();
    assert!(
        !unvouched.contains("the repo said no"),
        "an unvouched repository's hook did not run: {unvouched}"
    );
    assert!(
        unvouched.contains("the answer is 42"),
        "so the read happened: {unvouched}"
    );

    // Vouched for, the same file denies the same call.
    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure_trusting(home.path(), provider.port, workspace.path());

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let vouched = provider.request().to_string();
    assert!(
        vouched.contains("the repo said no"),
        "vouched for, it runs: {vouched}"
    );
}

/// The same configuration, plus the operator vouching for one directory.
fn configure_trusting(home: &Path, port: u16, workspace: &Path) {
    configure(home, port);
    let path = settings_path(home);
    let mut settings: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
        .expect("the settings file is JSON");
    settings["project"] = json!({
        workspace.display().to_string(): { "trust_level": "trusted" }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
}
