//! `arsy serve` end to end: a real client on a real pipe.
//!
//! The property under test is the one that makes serving safe at all — a
//! connected client holds no authority of its own, so a call policy does not
//! allow is answered with an error and nothing runs.

use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
};

/// A served session: the process, its stdin, and its answer stream.
struct Served {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Served {
    fn start(workspace: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arsy"))
            .args(["--workspace", workspace.to_str().unwrap()])
            // Never the Claude Code or Codex setup of the machine running the test.
            .env("CLAUDE_CONFIG_DIR", workspace.join("no-claude-home"))
            .env("CODEX_HOME", workspace.join("no-codex-home"))
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary runs");
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("an answer arrives");
        let answer: Value =
            serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("{error}: {line}"));
        assert_eq!(answer["id"], id, "answers are correlated");
        answer
    }

    fn notify(&mut self, method: &str) {
        let notification = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{notification}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn finish(mut self) {
        drop(self.stdin);
        let status = self.child.wait().expect("the server exits with its pipe");
        assert!(status.success(), "closing stdin ends the loop cleanly");
    }
}

fn workspace(policy: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(".arsy")).unwrap();
    // Written as TOML here and converted: the schema reads more clearly that
    // way, and what lands on disk is the `arsy.json` the binary loads.
    let settings = directory
        .path()
        .join(".arsy")
        .join(arsy_kernel::config::CONFIG_FILE);
    let body = format!("schema_version = 1\n{policy}");
    std::fs::write(
        &settings,
        arsy_kernel::config::json_from_toml(&body, &settings).unwrap(),
    )
    .unwrap();
    directory
}

#[test]
fn a_client_negotiates_lists_tools_and_is_refused_what_policy_denies() {
    // No policy rules at all: silence is a denial.
    let directory = workspace("");
    let mut served = Served::start(directory.path());

    let answer = served.call("initialize", json!({"protocolVersion": "2026-07-28"}));
    assert_eq!(answer["result"]["serverInfo"]["name"], "arsy");
    assert!(answer["result"]["capabilities"]["tools"].is_object());
    // A notification is never answered, so the next answer is the next call's.
    served.notify("notifications/initialized");

    let answer = served.call("tools/list", json!({}));
    let tools = answer["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "process.exec"));
    assert!(tools.iter().any(|tool| tool["name"] == "git.status"));

    let answer = served.call(
        "tools/call",
        json!({
            "name": "process.exec",
            "arguments": {
                "argv": ["/bin/echo", "hi"],
                "timeout_ms": 1_000,
                "max_output_bytes": 1_024,
            },
        }),
    );
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("process.exec"),
        "{answer}"
    );
    assert_eq!(answer["error"]["data"]["executed"], false);

    // Malformed input is a parse error the client can correlate to nothing,
    // and the loop keeps going.
    writeln!(served.stdin, "{{ not json").unwrap();
    served.stdin.flush().unwrap();
    let mut line = String::new();
    served.stdout.read_line(&mut line).unwrap();
    let answer: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32700);
    let answer = served.call("ping", json!({}));
    assert!(answer["result"].is_object(), "the loop survives bad input");

    served.finish();
}

/// A repository can ship policy, but it cannot grant itself authority: an
/// `allow` written in a workspace file is compiled down to a request for
/// approval, and a pipe has nobody to ask. Cloning a repository must not be a
/// way to get its tools run.
#[test]
fn a_workspace_allow_rule_cannot_authorize_a_served_call() {
    let directory = workspace(
        r#"
[[policy.rules]]
id = "exec"
effect = "allow"
action = "process.exec"
resource = "process:**"

[[policy.rules]]
id = "read-tree"
effect = "allow"
action = "git.read"
resource = "file:**"
"#,
    );
    let mut served = Served::start(directory.path());
    served.call("initialize", json!({}));

    for (name, arguments) in [
        (
            "process.exec",
            json!({"argv": ["/bin/echo", "hi"], "timeout_ms": 5_000, "max_output_bytes": 4_096}),
        ),
        ("git.status", json!({})),
    ] {
        let answer = served.call("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(answer["error"]["code"], -32020, "{answer}");
        assert_eq!(answer["error"]["data"]["executed"], false);
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("approval is required"),
            "{answer}"
        );
    }

    // A tool that does not exist is still not found, not refused.
    let answer = served.call("tools/call", json!({"name": "fs.obliterate"}));
    assert_eq!(answer["error"]["code"], -32601, "{answer}");

    served.finish();
}

/// The dry run and the served call must reach the same verdict. They only can
/// if they compile the same rules, which is why both go through
/// `Config::policy_rule_set` rather than assembling their own.
#[test]
fn policy_explain_and_a_served_call_agree() {
    let directory = workspace(
        r#"
[policy]
default_effect = "deny"

[[policy.rules]]
id = "read-tree"
effect = "allow"
action = "git.read"
resource = "file:**"
"#,
    );

    let explained = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", directory.path().to_str().unwrap()])
        // Never the Claude Code or Codex setup of the machine running the test.
        .env("CLAUDE_CONFIG_DIR", directory.path().join("no-claude-home"))
        .env("CODEX_HOME", directory.path().join("no-codex-home"))
        .args(["--output", "json"])
        .args(["policy", "explain", "git.status"])
        .output()
        .expect("the binary runs");
    let explained: Value = String::from_utf8(explained.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .expect("a result record")["payload"]
        .clone();
    // The workspace `allow` is downgraded to `ask`. Its matching rule takes
    // precedence over the fallback `deny`, so both paths require approval.
    assert_eq!(explained["decision"], "ask", "{explained}");
    assert_eq!(explained["executed"], false);
    assert_eq!(explained["evaluations"][0]["decision"]["effect"], "ask");

    let mut served = Served::start(directory.path());
    served.call("initialize", json!({}));
    let answer = served.call("tools/call", json!({"name": "git.status", "arguments": {}}));
    assert_eq!(answer["error"]["code"], -32020, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("approval is required"),
        "the served call reaches the verdict the dry run predicted: {answer}"
    );
    assert_eq!(answer["error"]["data"]["executed"], false);
    served.finish();
}
