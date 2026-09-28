//! The harness core, exercised the way a turn exercises it.
//!
//! Every assertion goes through [`ToolRuntime`] rather than the executors
//! underneath, because the thing worth protecting is the whole path: decode,
//! authorize, dispatch, read the result artifact back, render it. A test that
//! called an executor directly would pass while the model saw nothing.

use arsy_code::{
    agent::{self, Authorization, ExecutionMode, ToolRuntime},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::{ArtifactStore, FileArtifactStore},
    capability::{CapabilityAction, PolicySource, ResourcePattern},
    domain::{Principal, StateVersion},
    policy::{
        ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance,
        WorkspaceCleanliness,
    },
    safety::{SafetyDecision, TrustState},
};
use serde_json::{json, Value};
use std::sync::Arc;

/// A ruleset that allows `actions` outright and says nothing about the rest,
/// which the engine denies: silence is a refusal, so a test that forgets to
/// name an action gets a denial rather than a surprise effect.
fn rules(actions: &[CapabilityAction]) -> RuleSet {
    RuleSet::compile(actions.iter().map(|action| PolicyRule {
        source: PolicySource::User,
        effect: RuleEffect::Allow,
        actor: ActorMatch::Any,
        action: *action,
        pattern: ResourcePattern::new(action.default_scheme(), "**").unwrap(),
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    }))
}

fn runtime(root: &std::path::Path, rules: RuleSet) -> ToolRuntime {
    runtime_with_sandbox(root, rules, SandboxAssurance::None)
}

fn runtime_with_sandbox(
    root: &std::path::Path,
    rules: RuleSet,
    sandbox: SandboxAssurance,
) -> ToolRuntime {
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
            sandbox,
        },
        arsy_code::operations::Reachable::default(),
        "test",
        arsy_code::operations::TurnState::default(),
        &[],
    )
    .unwrap()
}

#[test]
fn safe_auto_reviews_exact_calls_and_keeps_an_audit_record() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime_with_sandbox(
        root.path(),
        rules(CapabilityAction::ALL),
        SandboxAssurance::Filesystem,
    );
    let intent = StateVersion::from_digest([7; 32]);

    let write = runtime
        .prepare("fs.write", &json!({"path": "safe.txt", "content": "ok"}))
        .unwrap();
    assert_eq!(
        runtime
            .review_auto(&write, intent, TrustState::Trusted, false)
            .decision,
        SafetyDecision::Allow
    );

    let delete = runtime
        .prepare("fs.delete", &json!({"path": "safe.txt"}))
        .unwrap();
    // Attended, Auto blocks a delete rather than stopping to ask.
    assert_eq!(
        runtime
            .review_auto(&delete, intent, TrustState::Trusted, false)
            .decision,
        SafetyDecision::Deny
    );
    assert_eq!(
        runtime
            .review_auto(&delete, intent, TrustState::Trusted, true)
            .decision,
        SafetyDecision::Deny
    );

    let audits = runtime.take_safety_audits();
    assert_eq!(audits.len(), 3);
    assert_eq!(audits[0].envelope.operation.as_str(), "fs.write");
    assert_eq!(audits[1].envelope.operation.as_str(), "fs.delete");
    assert_eq!(audits[2].result.decision, SafetyDecision::Deny);
}

/// A runtime that may do anything, for the tests about behaviour rather than
/// authority.
fn permissive(root: &std::path::Path) -> ToolRuntime {
    runtime(root, rules(CapabilityAction::ALL))
}

/// A permissive runtime that was handed a skill listing, the way a session
/// hands it the skills the prompt named.
fn with_skills(root: &std::path::Path, skills: &[agent::instructions::Skill]) -> ToolRuntime {
    let workspace = Workspace::open(root).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FileArtifactStore::open(root.join(".arsy/artifacts"), 0).unwrap());
    agent::runtime(
        &workspace,
        rules(CapabilityAction::ALL),
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
        skills,
    )
    .unwrap()
}

/// Run a call the way an operator at a keyboard would: whatever policy asks
/// for is answered yes.
///
/// [`ToolRuntime::invoke`] is the unattended path and refuses an approval, so
/// a test about what a tool *does* has to go the attended way — which is also
/// the path the TUI takes, so this exercises it.
fn attended(runtime: &ToolRuntime, tool: &str, arguments: &Value) -> agent::ToolResult {
    let Ok(request) = runtime.prepare(tool, arguments) else {
        return *runtime.prepare(tool, arguments).unwrap_err();
    };
    match runtime.authorize(&request).approve() {
        Ok(grants) => runtime.dispatch(tool, &request, &grants, std::time::Instant::now()),
        Err(reason) => agent::ToolResult::refused(tool, reason),
    }
}

fn ok(runtime: &ToolRuntime, tool: &str, arguments: Value) -> String {
    let result = attended(runtime, tool, &arguments);
    assert!(result.success, "{tool} failed: {}", result.output);
    result.output
}

fn err(runtime: &ToolRuntime, tool: &str, arguments: Value) -> String {
    let result = attended(runtime, tool, &arguments);
    assert!(
        !result.success,
        "{tool} unexpectedly succeeded: {}",
        result.output
    );
    result.output
}

#[test]
fn the_offered_tools_are_the_ones_the_registry_can_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    // A WASM build offers `plugin.invoke` as well; the rest of the list is the
    // same, and what this asserts is that the offer follows registration.
    let offered: Vec<String> = runtime
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .filter(|name| name != "plugin.invoke")
        .collect();
    assert_eq!(
        offered,
        [
            "fs.read",
            "fs.list",
            "search.files",
            "search.text",
            "code.symbol",
            "code.explain",
            "code.references",
            "code.diagnostics",
            "code.rename",
            "fs.edit",
            "apply_patch",
            "fs.write",
            "fs.delete",
            "fs.move",
            "bash",
            "web_fetch",
            "bash_start",
            "bash_poll",
            "bash_write",
            "bash_stop",
            "repo_map",
            "repo_discover",
            "git_status",
            "git_branch",
            "git_diff",
            "git_log",
            "git_blame",
            "plan_add",
            "plan_update",
            "plan_remove",
            "plan_reorder",
            "plan_list",
            // No `todo_*`: this runtime was built without a session journal,
            // and a durable checklist with nowhere to persist is not offered.
            "validate_record",
            "validate_status",
        ]
    );
    // Every offered tool publishes a schema the model can fill in, and no tool
    // is offered whose operation is not registered.
    for schema in runtime.schemas() {
        assert_eq!(schema.input_schema["type"], "object", "{}", schema.name);
        assert!(!schema.description.is_empty(), "{}", schema.name);
    }

    let unknown = err(&runtime, "grep", json!({}));
    assert!(
        unknown.contains("not a tool this session offers"),
        "{unknown}"
    );
    // The refusal names what is available, so the model can pick again rather
    // than guess a second time.
    assert!(unknown.contains("search.text"), "{unknown}");
}

#[test]
fn plan_mode_offers_exploration_and_planning_but_not_mutation_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path()).with_execution_mode(ExecutionMode::Plan);
    let offered: Vec<String> = runtime
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();

    for allowed in [
        "fs.read",
        "search.text",
        "code.explain",
        "git_status",
        "plan_add",
        "plan_list",
        // Reading the repository is what planning is made of.
        "repo_map",
        // Reading a page is part of making a plan, and changes nothing.
        "web_fetch",
    ] {
        assert!(offered.iter().any(|name| name == allowed), "{allowed}");
    }
    for blocked in [
        "fs.edit",
        "apply_patch",
        "fs.write",
        "fs.delete",
        "fs.move",
        "code.rename",
        "plugin.invoke",
        "bash",
        "bash_start",
        "bash_write",
        "bash_stop",
    ] {
        assert!(!offered.iter().any(|name| name == blocked), "{blocked}");
    }
}

#[test]
fn plan_mode_blocks_policy_allowed_writes_at_authorization_and_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("kept.txt");
    std::fs::write(&path, "before\n").unwrap();
    let normal = permissive(root.path());
    let plan = permissive(root.path()).with_execution_mode(ExecutionMode::Plan);
    let arguments = json!({"path": "kept.txt", "content": "after\n"});
    assert_eq!(
        plan.invoke("fs.read", &json!({"path": "kept.txt"})).output,
        "   1 │ before"
    );
    let request = normal.prepare("fs.write", &arguments).unwrap();
    let grants = normal.authorize(&request).approve().unwrap();

    assert!(matches!(plan.authorize(&request), Authorization::Denied(_)));
    let invoked = plan.invoke("fs.write", &arguments);
    assert!(!invoked.success);
    assert!(invoked.output.contains("Blocked in Plan Mode"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
    let result = plan.dispatch("fs.write", &request, &grants, std::time::Instant::now());
    assert!(!result.success);
    assert!(
        result.output.contains("Blocked in Plan Mode"),
        "{}",
        result.output
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
}

/// Not being offered a tool is not the same as being refused one: a model can
/// name a tool it was never shown, and a front end holds grants minted before
/// the mode changed. Every mutation this build has is put through the gate
/// itself, so the ceiling is the operation's contract rather than a list of
/// names that a new tool could be added without.
#[test]
fn plan_mode_refuses_every_mutating_tool_at_the_gate_not_only_in_the_offered_list() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("kept.txt"), "before\n").unwrap();
    std::fs::write(root.path().join("other.txt"), "other\n").unwrap();
    let normal = permissive(root.path());
    let plan = permissive(root.path()).with_execution_mode(ExecutionMode::Plan);

    for (name, arguments) in [
        (
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Delete File: other.txt\n*** End Patch\n"}),
        ),
        (
            "fs.edit",
            json!({"path": "kept.txt", "old_text": "before", "new_text": "after"}),
        ),
        ("fs.delete", json!({"path": "kept.txt"})),
        ("fs.move", json!({"from": "kept.txt", "to": "moved.txt"})),
        (
            "code.rename",
            json!({"symbol": "symbol:kept.txt#thing", "new_name": "other"}),
        ),
        ("bash", json!({"command": "rm -f kept.txt"})),
    ] {
        let request = plan.prepare(name, &arguments).unwrap();
        assert!(
            matches!(plan.authorize(&request), Authorization::Denied(_)),
            "{name} was authorized in Plan Mode"
        );

        // The grants a Normal-mode turn would have held, spent against the
        // Plan-mode runtime: dispatch refuses them on their own.
        let held = normal.prepare(name, &arguments).unwrap();
        let grants = normal
            .authorize(&held)
            .approve()
            .unwrap_or_else(|error| panic!("{name} could not be granted normally: {error}"));
        let dispatched = plan.dispatch(name, &held, &grants, std::time::Instant::now());
        assert!(!dispatched.success, "{name} ran in Plan Mode");
        assert!(
            dispatched.output.contains("Blocked in Plan Mode"),
            "{name}: {}",
            dispatched.output
        );
    }

    // Nothing on disk moved, in either direction.
    assert_eq!(
        std::fs::read_to_string(root.path().join("kept.txt")).unwrap(),
        "before\n"
    );
    assert!(root.path().join("other.txt").exists());
    assert!(!root.path().join("moved.txt").exists());
}

#[test]
fn reading_returns_text_line_windows_and_reports_binary_without_decoding_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    std::fs::write(root.path().join("blob"), b"\x00\x01binary").unwrap();
    let runtime = permissive(root.path());

    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "src/lib.rs"})),
        "   1 │ one\n   2 │ two\n   3 │ three\n   4 │ four"
    );

    // A window says which lines it is, so a following edit can be addressed.
    let window = ok(
        &runtime,
        "fs.read",
        json!({"path": "src/lib.rs", "offset": 2, "limit": 2}),
    );
    assert_eq!(window, "lines 2-3 of 4\n   2 │ two\n   3 │ three");

    let binary = ok(&runtime, "fs.read", json!({"path": "blob"}));
    assert!(binary.contains("binary file"), "{binary}");
    assert!(!binary.contains('\u{fffd}'), "{binary}");

    let missing = err(&runtime, "fs.read", json!({"path": "nowhere.rs"}));
    assert!(!missing.is_empty());

    // Nothing to return is two different questions, and an empty string would
    // be read as "the file is empty" for both.
    std::fs::write(root.path().join("empty.rs"), "").unwrap();
    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "empty.rs"})),
        "(empty file)"
    );
    let past = ok(
        &runtime,
        "fs.read",
        json!({"path": "src/lib.rs", "offset": 900}),
    );
    assert_eq!(past, "(offset 900 is past the end; the file has 4 lines)");
}

/// The operator's own home declares skills too, and their `SKILL.md` is
/// absolute. The prompt lists them, so `skill://` has to open one — a listing
/// naming a file the model cannot read is worse than no listing.
#[test]
fn a_skill_declared_outside_the_workspace_is_read_where_it_lives() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let declared = home.path().join("skills/review/SKILL.md");
    std::fs::create_dir_all(declared.parent().unwrap()).unwrap();
    std::fs::write(&declared, "---\nname: review\n---\nhow to review").unwrap();
    let runtime = with_skills(
        root.path(),
        &[agent::instructions::Skill {
            name: "review".to_owned(),
            ecosystem: "claude".to_owned(),
            path: declared.display().to_string(),
            description: None,
        }],
    );

    let body = ok(&runtime, "fs.read", json!({"path": "skill://review"}));
    assert!(body.contains("how to review"), "{body}");

    // The reference is what opens it, not the path: the same file named
    // directly is outside the workspace and stays refused.
    let direct = err(
        &runtime,
        "fs.read",
        json!({"path": declared.display().to_string()}),
    );
    assert!(!direct.is_empty());

    // A name nothing declared is an error, not a read of the literal string.
    let unknown = err(&runtime, "fs.read", json!({"path": "skill://nothing"}));
    assert!(unknown.contains("nothing"), "{unknown}");
}

#[test]
fn listing_sorts_directories_first_and_names_the_root_by_default() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("b.txt"), "bb").unwrap();
    std::fs::write(root.path().join("a.txt"), "a").unwrap();
    let runtime = permissive(root.path());

    let listed = ok(&runtime, "fs.list", json!({}));
    let rows: Vec<&str> = listed.lines().collect();
    assert_eq!(rows[0], "src/");
    assert!(rows.contains(&"a.txt (1 bytes)"), "{listed}");
    assert!(rows.contains(&"b.txt (2 bytes)"), "{listed}");

    assert_eq!(
        ok(&runtime, "fs.list", json!({"path": "src"})),
        "(empty directory)"
    );
}

#[test]
fn search_finds_files_by_glob_and_text_by_content_honouring_gitignore() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/deep")).unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "fn authenticate() {}\n").unwrap();
    std::fs::write(root.path().join("src/deep/mod.rs"), "// authenticate\n").unwrap();
    std::fs::write(root.path().join("ignored.rs"), "fn authenticate() {}\n").unwrap();
    let runtime = permissive(root.path());

    // A pattern without a separator matches file names at any depth.
    let found = ok(&runtime, "search.files", json!({"pattern": "*.rs"}));
    let paths: Vec<&str> = found.lines().collect();
    assert_eq!(paths, ["src/deep/mod.rs", "src/lib.rs"], "{found}");
    assert_eq!(
        ok(&runtime, "search.files", json!({"pattern": "src/*.rs"})),
        "src/lib.rs"
    );

    let hits = ok(&runtime, "search.text", json!({"query": "authenticate"}));
    assert!(hits.contains("src/lib.rs:1:"), "{hits}");
    assert!(
        !hits.contains("ignored.rs"),
        "an ignored file is not searched: {hits}"
    );

    assert_eq!(
        ok(&runtime, "search.text", json!({"query": "nothing here"})),
        "no matches"
    );
    // A limit that cuts the answer says so, rather than looking complete.
    let limited = ok(
        &runtime,
        "search.text",
        json!({"query": "authenticate", "limit": 1}),
    );
    assert!(limited.contains("more results were available"), "{limited}");
}

#[test]
fn editing_replaces_a_unique_anchor_and_refuses_an_ambiguous_one() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rs"), "let a = 1;\nlet b = 1;\n").unwrap();
    let runtime = permissive(root.path());

    let edited = attended(
        &runtime,
        "fs.edit",
        &json!({"path": "main.rs", "old_text": "let a = 1;", "new_text": "let a = 2;"}),
    );
    assert!(edited.success, "{}", edited.output);
    assert!(
        edited.output.contains("-   1 │ let a = 1;"),
        "{}",
        edited.output
    );
    assert!(
        edited.output.contains("+   1 │ let a = 2;"),
        "{}",
        edited.output
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b = 1;\n"
    );

    // Two candidates and no choice between them is refused, not guessed.
    let ambiguous = err(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": " = ", "new_text": " := "}),
    );
    assert!(ambiguous.contains("candidates"), "{ambiguous}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b = 1;\n",
        "a refused edit leaves the file alone"
    );

    // Naming the occurrence resolves it.
    ok(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": " = ", "new_text": " := ", "occurrence": 2}),
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b := 1;\n"
    );

    let absent = err(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": "not present", "new_text": "x"}),
    );
    assert!(absent.contains("anchor"), "{absent}");
}

#[test]
fn writing_creating_moving_and_deleting_report_what_they_changed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    let written = attended(
        &runtime,
        "fs.write",
        &json!({"path": "nested/deep/file.txt", "content": "hello"}),
    );
    assert!(written.success, "{}", written.output);
    assert_eq!(written.changed_files, ["nested/deep/file.txt"]);
    assert!(
        written.output.contains("+   1 │ hello"),
        "{}",
        written.output
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/deep/file.txt")).unwrap(),
        "hello"
    );

    // Writing over the same path reports an update, not a creation, so the
    // model is not told it made a file it replaced.
    let again = attended(
        &runtime,
        "fs.write",
        &json!({"path": "nested/deep/file.txt", "content": "hello again"}),
    );
    assert!(again.success, "{}", again.output);
    assert_eq!(again.metadata["created"], false);
    assert!(again.output.starts_with("updated"), "{}", again.output);

    ok(
        &runtime,
        "fs.move",
        json!({"from": "nested/deep/file.txt", "to": "moved.txt"}),
    );
    assert!(!root.path().join("nested/deep/file.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("moved.txt")).unwrap(),
        "hello again"
    );

    // A move never silently replaces the destination.
    std::fs::write(root.path().join("occupied.txt"), "keep me").unwrap();
    let clash = err(
        &runtime,
        "fs.move",
        json!({"from": "moved.txt", "to": "occupied.txt"}),
    );
    assert!(clash.contains("already exists"), "{clash}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("occupied.txt")).unwrap(),
        "keep me"
    );

    assert_eq!(
        ok(&runtime, "fs.delete", json!({"path": "moved.txt"})),
        "deleted moved.txt"
    );
    assert!(!root.path().join("moved.txt").exists());
}

#[test]
fn a_patch_adds_updates_moves_and_deletes_and_is_refused_when_it_cannot_be_placed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    let added = ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {\n+    println!(\"one\");\n+}\n*** End Patch\n"}),
    );
    assert_eq!(added, "added src/main.rs");
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
        "fn main() {\n    println!(\"one\");\n}\n"
    );

    // Context anchors the change: the `-` line is found by the lines around it.
    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main\n fn main() {\n-    println!(\"one\");\n+    println!(\"two\");\n }\n*** End Patch\n"}),
    );
    assert!(std::fs::read_to_string(root.path().join("src/main.rs"))
        .unwrap()
        .contains("two"));

    // Indentation that drifted still matches, after the exact pass fails.
    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n-  println!(\"two\");\n+    println!(\"three\");\n*** End Patch\n"}),
    );
    assert!(std::fs::read_to_string(root.path().join("src/main.rs"))
        .unwrap()
        .contains("three"));

    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n*** Move to: src/app.rs\n-}\n+}\n*** End Patch\n"}),
    );
    assert!(!root.path().join("src/main.rs").exists());
    assert!(root.path().join("src/app.rs").exists());

    assert_eq!(
        ok(
            &runtime,
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Delete File: src/app.rs\n*** End Patch\n"})
        ),
        "deleted src/app.rs"
    );

    std::fs::write(root.path().join("file.txt"), "alpha\nbeta\n").unwrap();
    let stale = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: file.txt\n-gamma\n+delta\n*** End Patch\n"}),
    );
    assert!(stale.contains("no line matched"), "{stale}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "alpha\nbeta\n",
        "a rejected patch leaves the file alone"
    );

    let unmarked = err(&runtime, "apply_patch", json!({"patch": "just text"}));
    assert!(unmarked.contains("*** Begin Patch"), "{unmarked}");
}

#[test]
fn a_failed_hunk_names_the_closest_line_so_the_model_can_recover() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("runtime.rs"),
        "fn build_context(session: &Session) {\n    todo!()\n}\n",
    )
    .unwrap();
    let runtime = permissive(root.path());

    let rejected = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: runtime.rs\n-fn build_context(session: &mut Session) {\n+fn build_context(session: &Session, budget: u32) {\n*** End Patch\n"}),
    );
    assert!(rejected.contains("no line matched"), "{rejected}");
    assert!(
        rejected.contains("the closest is line 1"),
        "a rejected hunk points at what is actually there: {rejected}"
    );
    assert!(
        rejected.contains("fn build_context(session: &Session)"),
        "{rejected}"
    );
}

#[test]
fn a_patch_that_half_applies_says_what_already_landed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("first.txt"), "alpha\n").unwrap();
    std::fs::write(root.path().join("second.txt"), "beta\n").unwrap();
    let runtime = permissive(root.path());

    // The first file matches; the second does not.
    let failed = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: first.txt\n-alpha\n+ALPHA\n*** Update File: second.txt\n-gamma\n+GAMMA\n*** End Patch\n"}),
    );

    assert!(failed.contains("no line matched"), "{failed}");
    assert!(
        failed.contains("already made") && failed.contains("updated first.txt"),
        "a half-applied patch must name what is on disk, or the model retries \
         the whole thing against files it already changed: {failed}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("first.txt")).unwrap(),
        "ALPHA\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("second.txt")).unwrap(),
        "beta\n"
    );
}

#[test]
fn no_tool_reaches_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("target.txt"), "outside\n").unwrap();
    let runtime = permissive(root.path());

    for escape in ["../outside.txt", "/etc/passwd"] {
        for (tool, arguments) in [
            ("fs.read", json!({"path": escape})),
            ("fs.write", json!({"path": escape, "content": "x"})),
            ("fs.delete", json!({"path": escape})),
        ] {
            let refused = err(&runtime, tool, arguments);
            assert!(
                refused.contains("outside the workspace") || refused.contains("path"),
                "{tool} {escape}: {refused}"
            );
        }
    }

    // A symlink is a local-looking name that resolves out of the tree, so the
    // lexical check passes it and the capability directory has to catch it.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("link")).unwrap();
        let escaped = err(
            &runtime,
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: link/target.txt\n-outside\n+captured\n*** End Patch\n"}),
        );
        assert!(!escaped.is_empty());
        assert_eq!(
            std::fs::read_to_string(elsewhere.path().join("target.txt")).unwrap(),
            "outside\n",
            "the file outside the workspace is untouched"
        );
    }
}

/// The commands are POSIX and the tool spawns `sh`, so this is a unix test —
/// the same gate `process::tests` uses, and for the same reason.
#[cfg(unix)]
#[test]
fn bash_runs_in_the_workspace_and_reports_output_exit_codes_and_deadlines() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("marker"), "x").unwrap();
    let runtime = permissive(root.path());

    let hi = ok(&runtime, "bash", json!({"command": "printf 'hi\\n'"}));
    assert!(hi.starts_with("hi\n"), "{hi}");
    // Every successful call ends with the evidence line `validate_record`
    // reads its exit code from.
    assert!(hi.contains("\nevidence: "), "{hi}");
    // The workspace is the working directory, not wherever ARSY was launched.
    assert!(ok(&runtime, "bash", json!({"command": "ls"})).contains("marker"));

    let failed = err(
        &runtime,
        "bash",
        json!({"command": "printf 'oops\\n' >&2; exit 3"}),
    );
    assert!(failed.contains("oops"), "{failed}");
    assert!(failed.contains("Command exited with code 3"), "{failed}");
    assert!(failed.contains("\nevidence: "), "{failed}");

    // The deadline in the message is the proof: a command that was allowed to
    // finish would report its own exit, not a deadline.
    let timed_out = err(
        &runtime,
        "bash",
        json!({"command": "trap '' TERM; sleep 30", "timeout_ms": 300}),
    );
    assert!(timed_out.contains("deadline"), "{timed_out}");

    // Output beyond the cap keeps the tail, which is where a failure is.
    let long = ok(
        &runtime,
        "bash",
        json!({"command": "seq 1 100000; printf 'LAST\\n'"}),
    );
    assert!(
        long.len() < agent::MAX_TOOL_OUTPUT_BYTES + 128,
        "{}",
        long.len()
    );
    assert!(long.contains("earlier bytes omitted"), "{long}");
    assert!(long.contains("LAST\n"), "{long}");
    assert!(long.contains("\nevidence: "), "{long}");

    let empty = err(&runtime, "bash", json!({"command": "   "}));
    assert!(empty.contains("non-empty"), "{empty}");
}

#[test]
fn policy_decides_every_call_and_a_refusal_performs_nothing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("secret.txt"), "before").unwrap();
    // Reading is allowed; writing, deleting, and executing are not named, and
    // silence is a denial.
    let runtime = runtime(root.path(), rules(&[CapabilityAction::FsRead]));

    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "secret.txt"})),
        "   1 │ before"
    );

    for (tool, arguments) in [
        (
            "fs.write",
            json!({"path": "secret.txt", "content": "after"}),
        ),
        ("fs.delete", json!({"path": "secret.txt"})),
        ("bash", json!({"command": "rm secret.txt"})),
    ] {
        let refused = err(&runtime, tool, arguments);
        assert!(refused.contains("denied by policy"), "{tool}: {refused}");
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "before",
        "a denied call performed nothing"
    );
}

#[test]
fn an_approval_grants_exactly_what_was_shown_and_nothing_beside_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "a").unwrap();
    std::fs::write(root.path().join("b.txt"), "b").unwrap();
    let asks = RuleSet::compile([PolicyRule {
        source: PolicySource::User,
        effect: RuleEffect::RequireApproval,
        actor: ActorMatch::Any,
        action: CapabilityAction::FsWrite,
        pattern: ResourcePattern::new(CapabilityAction::FsWrite.default_scheme(), "**").unwrap(),
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    }]);
    let runtime = runtime(root.path(), asks);

    // Unattended, the call is refused rather than run: `invoke` is the path a
    // pipeline takes, and it has no operator to ask.
    let unattended = runtime.invoke("fs.write", &json!({"path": "a.txt", "content": "changed"}));
    assert!(!unattended.success, "{}", unattended.output);
    assert!(
        unattended.output.contains("approval"),
        "{}",
        unattended.output
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "a"
    );

    // With one, the grant covers the file that was shown.
    let request = runtime
        .prepare("fs.write", &json!({"path": "a.txt", "content": "changed"}))
        .unwrap();
    let authorization = runtime.authorize(&request);
    assert!(matches!(authorization, Authorization::NeedsApproval { .. }));
    let grants = authorization.approve().unwrap();
    let result = runtime.dispatch("fs.write", &request, &grants, std::time::Instant::now());
    assert!(result.success, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "changed"
    );

    // The same grants do not carry to another file: an approval is not a mode.
    let other = runtime
        .prepare("fs.write", &json!({"path": "b.txt", "content": "changed"}))
        .unwrap();
    let leaked = runtime.dispatch("fs.write", &other, &grants, std::time::Instant::now());
    assert!(!leaked.success, "{}", leaked.output);
    assert_eq!(
        std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "b"
    );
}

#[test]
fn instructions_are_discovered_root_first_and_only_where_they_belong() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("crates/inner")).unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "root rule").unwrap();
    // Claude Code reads CLAUDE.md beside AGENTS.md, so both contribute.
    std::fs::write(root.path().join("CLAUDE.md"), "claude rule").unwrap();
    // An override replaces AGENTS.md, and a copy of it is not read twice.
    std::fs::create_dir_all(root.path().join("crates")).unwrap();
    std::fs::write(root.path().join("crates/AGENTS.md"), "replaced").unwrap();
    std::fs::write(
        root.path().join("crates/AGENTS.override.md"),
        "override rule",
    )
    .unwrap();
    std::fs::write(root.path().join("crates/CLAUDE.md"), "override rule").unwrap();
    std::fs::write(root.path().join("crates/inner/CLAUDE.md"), "nested rule").unwrap();
    // Ordinary documentation is not an instruction.
    std::fs::write(root.path().join("README.md"), "not an instruction").unwrap();
    std::fs::write(root.path().join("crates/notes.md"), "also not").unwrap();

    let workspace = Workspace::open(root.path()).unwrap();
    let found = agent::instructions::discover(&workspace, &root.path().join("crates/inner"));

    assert_eq!(
        found
            .iter()
            .map(|instruction| (instruction.path.as_str(), instruction.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("AGENTS.md", "root rule"),
            ("CLAUDE.md", "claude rule"),
            ("crates/AGENTS.override.md", "override rule"),
            ("crates/inner/CLAUDE.md", "nested rule"),
        ],
        "root first, agents file then CLAUDE.md, and nothing that is merely Markdown"
    );
    let without_claude =
        agent::instructions::discover_with(&workspace, &root.path().join("crates/inner"), false);
    assert!(without_claude
        .iter()
        .all(|instruction| !instruction.path.ends_with("CLAUDE.md")));

    // The operator's own file is labelled as theirs, not the project's.
    let mut found = found;
    found.insert(
        0,
        agent::instructions::Instruction {
            path: "/home/op/.claude/CLAUDE.md".to_owned(),
            text: "operator rule".to_owned(),
            truncated: false,
            operator: true,
        },
    );
    let prompt = agent::instructions::system_prompt(
        arsy_kernel::prompt::ModelFamily::Claude,
        &found,
        &[],
        &[],
        None,
        ExecutionMode::Normal,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .unwrap();
    let rendered = agent::instructions::render(&prompt);
    assert!(rendered.contains("root rule"), "{rendered}");
    assert!(
        rendered.contains("<user-instructions path=\"/home/op/.claude/CLAUDE.md\">\noperator rule"),
        "{rendered}"
    );
    assert!(rendered.contains("nested rule"), "{rendered}");
    assert!(rendered.contains(agent::instructions::HARNESS_INSTRUCTIONS.trim_end()));
    assert!(
        !rendered.contains("not an instruction"),
        "README is not injected: {rendered}"
    );

    let plan = agent::instructions::system_prompt(
        arsy_kernel::prompt::ModelFamily::Claude,
        &found,
        &[],
        &[],
        None,
        ExecutionMode::Plan,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .unwrap();
    let rendered = agent::instructions::render(&plan);
    assert!(rendered.contains("You are in Plan Mode"), "{rendered}");
    assert!(rendered.contains("Do not execute the plan"), "{rendered}");

    // `plugin.invoke` takes an id, and its schema cannot say which ids exist:
    // without this listing the model has a tool it could only call by guessing.
    let with_plugin = agent::instructions::system_prompt(
        arsy_kernel::prompt::ModelFamily::Claude,
        &found,
        &[agent::instructions::ExtensionTool {
            id: "formatter".to_owned(),
            version: "1.2.0".to_owned(),
            capabilities: vec!["fs.read".to_owned()],
        }],
        &[],
        None,
        ExecutionMode::Normal,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .unwrap();
    let rendered = agent::instructions::render(&with_plugin);
    assert!(
        rendered.contains("`formatter` (version 1.2.0)"),
        "{rendered}"
    );
    assert!(rendered.contains("granted fs.read"), "{rendered}");
    assert!(
        rendered.contains("plugin.invoke"),
        "the listing names the tool that calls them: {rendered}"
    );
}

/// A plugin an operator installed is callable in the build they installed it
/// with — no feature flag, no rebuild, no second tool surface.
#[cfg(feature = "wasm")]
#[test]
fn an_installed_plugin_is_callable_through_the_ordinary_tool_path() {
    let root = tempfile::tempdir().unwrap();

    // A source directory as `arsy plugin install` would be pointed at. The
    // module reads the input a byte at a time and emits each byte back.
    let source = root.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("plugin.toml"),
        "manifest_version = 1\n\
         id = \"echo\"\n\
         version = \"1.0.0\"\n\
         entrypoint = \"plugin.wasm\"\n\
         api = \"1\"\n\
         capabilities = []\n\
         imports = [\"arsy::read_input_byte\", \"arsy::emit_byte\"]\n",
    )
    .unwrap();
    std::fs::write(
        source.join("plugin.wasm"),
        r#"
        (module
          (import "arsy" "read_input_byte" (func $read (param i32) (result i32)))
          (import "arsy" "emit_byte" (func $emit (param i32) (result i32)))
          (memory 1)
          (func (export "run")
            (local $index i32)
            (local $byte i32)
            (block $done
              (loop $next
                (local.set $byte (call $read (local.get $index)))
                (br_if $done (i32.lt_s (local.get $byte) (i32.const 0)))
                (drop (call $emit (local.get $byte)))
                (local.set $index (i32.add (local.get $index) (i32.const 1)))
                (br $next)))))
        "#,
    )
    .unwrap();

    let registry = arsy_code::plugin::Registry::open(root.path());
    let (manifest, _) = arsy_code::plugin::Registry::inspect_source(&source).unwrap();
    // What the operator approved, which is exactly what the manifest asked for.
    let approved = arsy_code::plugin::Grant::for_manifest(&manifest, 0);
    let installed = registry.install(&source, &approved, 0).unwrap();
    assert!(installed.loadable());

    // The same runtime a turn gets, offering the same generic operation.
    let runtime = permissive(root.path());
    assert!(
        runtime
            .schemas()
            .iter()
            .any(|schema| schema.name == "plugin.invoke"),
        "the supported build offers the plugin tool without a feature flag"
    );

    let result = attended(
        &runtime,
        "plugin.invoke",
        &json!({"plugin": "echo", "input": "round trip"}),
    );
    assert!(result.success, "{}", result.output);
    assert!(
        result.output.contains("round trip"),
        "the plugin's own output reaches the model: {}",
        result.output
    );
    assert!(
        result.artifact.is_some(),
        "a plugin call leaves the same evidence every other call does"
    );

    // An id nobody installed is a refusal that says so, not a crash.
    let missing = err(&runtime, "plugin.invoke", json!({"plugin": "absent"}));
    assert!(missing.contains("absent"), "{missing}");
}

/// Independent reads run together; anything that writes runs alone, in order.
#[test]
fn a_batch_runs_independent_reads_together_and_serializes_everything_else() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..6 {
        std::fs::write(
            root.path().join(format!("f{index}.txt")),
            format!("body {index}"),
        )
        .unwrap();
    }
    let runtime = permissive(root.path());

    // Six reads: independent, so all six may run at once.
    let reads: Vec<(String, Value)> = (0..6)
        .map(|index| {
            (
                "fs.read".to_owned(),
                json!({"path": format!("f{index}.txt")}),
            )
        })
        .collect();
    let results = runtime.invoke_batch(&reads, 6);
    assert_eq!(results.len(), reads.len());
    for (index, result) in results.iter().enumerate() {
        assert!(result.success, "{}", result.output);
        assert_eq!(
            result.output,
            format!("   1 │ body {index}"),
            "a result is paired to its own call by position, whatever order it finished in"
        );
    }

    // A write in the middle: it is not batchable, so it runs on its own and
    // everything keeps its place.
    let mixed = vec![
        ("fs.read".to_owned(), json!({"path": "f0.txt"})),
        (
            "fs.write".to_owned(),
            json!({"path": "f0.txt", "content": "rewritten"}),
        ),
        ("fs.read".to_owned(), json!({"path": "f0.txt"})),
    ];
    let results = runtime.invoke_batch(&mixed, 4);
    assert_eq!(
        results[0].output, "   1 │ body 0",
        "the read before the write"
    );
    assert!(results[1].success);
    assert_eq!(
        results[2].output, "   1 │ rewritten",
        "a write is serialized against the reads around it, so ordering holds"
    );

    assert!(runtime.is_observational("fs.read", &json!({"path": "f0.txt"})));
    assert!(!runtime.is_observational("fs.write", &json!({"path": "f0.txt", "content": "x"})));
    assert!(
        !runtime.is_observational("bash", &json!({"command": "ls"})),
        "a shell command may write anywhere, so it never joins a batch"
    );
    assert!(
        !runtime.is_observational("nonsense", &json!({})),
        "a call that cannot be decoded fails on its own, where its error is the only outcome"
    );

    // A limit of one is still correct, only sequential: the same answers, in
    // the same places.
    let reads: Vec<(String, Value)> = (1..6)
        .map(|index| {
            (
                "fs.read".to_owned(),
                json!({"path": format!("f{index}.txt")}),
            )
        })
        .collect();
    let sequential: Vec<String> = runtime
        .invoke_batch(&reads, 1)
        .into_iter()
        .map(|result| result.output)
        .collect();
    let concurrent: Vec<String> = runtime
        .invoke_batch(&reads, 5)
        .into_iter()
        .map(|result| result.output)
        .collect();
    assert_eq!(sequential, concurrent);
    assert_eq!(sequential[0], "   1 │ body 1");
}

#[test]
fn every_tool_result_is_recoverable_evidence_in_the_artifact_store() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "content").unwrap();
    let runtime = permissive(root.path());

    let result = attended(&runtime, "fs.read", &json!({"path": "file.txt"}));
    assert!(result.success);
    // The rendered text is for the model; the structured result is what an
    // audit reads, and it carries the digest a later edit is checked against.
    assert_eq!(result.metadata["path"], "file.txt");
    assert_eq!(result.metadata["total_lines"], 1);
    assert!(result.metadata["digest"]
        .as_str()
        .is_some_and(|digest| !digest.is_empty()));
}
