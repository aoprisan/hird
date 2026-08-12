//! The dispatch hook, end to end: a configured command hears about every task
//! that becomes claimable, from the CLI verbs and the MCP tools alike.
//!
//! The hook here appends one line per announcement to a log file. That is the
//! whole contract under test: hird runs the command detached with the
//! announcement in its environment, and what the command does with it —
//! prompting an idle agent being the case the feature exists for — is not
//! hird's business.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;
use support::{McpSession, Sandbox};

/// A sandbox whose config carries a dispatch hook, and the log it writes.
fn hooked_sandbox() -> (Sandbox, PathBuf) {
    let sandbox = Sandbox::new();
    let log = sandbox.dir.path().join("herald.log");
    sandbox.write_config(&format!(
        "dispatch_hook = \"echo \\\"$HIRD_EVENT $HIRD_TASK $HIRD_TITLE [$HIRD_RECUSED]\\\" >> {}\"\n",
        log.display()
    ));
    (sandbox, log)
}

/// Wait for `needle` to land in the log. The hook runs detached, so the write
/// can trail the command that fired it.
fn wait_for(log: &Path, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = std::fs::read_to_string(log).unwrap_or_default();
        if contents.contains(needle) {
            return contents;
        }
        assert!(
            Instant::now() < deadline,
            "the hook never wrote {needle:?}; log so far:\n{contents}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn filing_and_replanning_announce_exactly_the_tasks_left_claimable() {
    let (sandbox, log) = hooked_sandbox();

    // Filed with nothing blocking it: announced.
    let gate: i64 = sandbox.run(&["add", "gate"]).trim().parse().unwrap();
    wait_for(&log, &format!("filed {gate} gate"));

    // Filed behind the gate: not claimable, so nothing to announce.
    let blocked: i64 = sandbox
        .run(&["add", "blocked", "--needs", &gate.to_string()])
        .trim()
        .parse()
        .unwrap();

    // Removing the edge is the human re-plan that leaves it waiting for
    // nothing.
    sandbox.run(&[
        "dep",
        "rm",
        &blocked.to_string(),
        "--needs",
        &gate.to_string(),
    ]);
    let contents = wait_for(&log, &format!("unblocked {blocked} blocked"));
    assert!(
        !contents.contains(&format!("filed {blocked}")),
        "a task filed behind a gate must not be announced at filing:\n{contents}"
    );

    // Cancel and reopen: the reopen puts it back in front of agents.
    sandbox.run(&["cancel", &blocked.to_string(), "--reason", "replanning"]);
    sandbox.run(&["reopen", &blocked.to_string(), "--reason", "back on"]);
    wait_for(&log, &format!("reopened {blocked} blocked"));
}

#[test]
fn a_plan_announces_its_first_wave_and_only_that() {
    let (sandbox, log) = hooked_sandbox();
    sandbox.write_file(
        "port.toml",
        r#"
plan = "port"

[[task]]
name = "schema"
title = "define the schema"

[[task]]
name = "loader"
title = "port the loader"
needs = ["schema"]
"#,
    );
    let filed = sandbox.run(&["plan", "apply", "port.toml"]);
    assert!(filed.contains("filed 2 tasks"), "{filed}");

    let contents = wait_for(&log, "filed 1 define the schema");
    assert!(
        !contents.contains("port the loader"),
        "the second wave is not claimable and must stay quiet:\n{contents}"
    );
}

#[test]
fn a_finish_announces_the_dependents_it_released_and_the_review_it_filed() {
    let (sandbox, log) = hooked_sandbox();
    sandbox.git_init();

    // A declared scope is what makes the finish file a review: a review of
    // work that names no files would have nothing to read.
    let gate: i64 = sandbox
        .run(&[
            "add",
            "port the loader",
            "--review",
            "--path",
            "src/loader.rs",
        ])
        .trim()
        .parse()
        .unwrap();
    let dependent: i64 = sandbox
        .run(&["add", "use the loader", "--needs", &gate.to_string()])
        .trim()
        .parse()
        .unwrap();

    let mut claude = McpSession::start(&sandbox, "claude-code");
    claude.call("task_claim", json!({"seq": gate})).unwrap();
    let done = claude
        .call("task_complete", json!({"seq": gate, "result": "ported"}))
        .unwrap();
    let review = done["review_filed"].as_i64().expect("a review was filed");

    // One finish, two announcements: the dependent it unblocked — for anyone,
    // so `HIRD_RECUSED` is empty — and the review it filed, announced with
    // the author's harness barred so a routing hook never summons the one
    // agent the queue is about to turn away.
    wait_for(&log, &format!("unblocked {dependent} use the loader []"));
    wait_for(
        &log,
        &format!("review_filed {review} Review: port the loader [claude-code]"),
    );

    // A sent-back verdict announces the reopened work.
    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": review})).unwrap();
    codex
        .call(
            "task_complete",
            json!({
                "seq": review,
                "result": "the error path drops the lock; take it before the retry",
                "verdict": "sent_back",
            }),
        )
        .unwrap();
    // The reopened work bars nobody — a redo may land on anyone, its author
    // included.
    wait_for(&log, &format!("sent_back {gate} port the loader []"));

    claude.shutdown();
    codex.shutdown();
}

#[test]
fn a_release_announces_the_task_it_hands_back() {
    let (sandbox, log) = hooked_sandbox();
    let seq: i64 = sandbox.run(&["add", "contended"]).trim().parse().unwrap();
    wait_for(&log, &format!("filed {seq} contended"));

    let mut claude = McpSession::start(&sandbox, "claude-code");
    claude.call("task_claim", json!({"seq": seq})).unwrap();
    claude
        .call(
            "task_release",
            json!({"seq": seq, "reason": "out of context"}),
        )
        .unwrap();
    wait_for(&log, &format!("released {seq} contended"));
    claude.shutdown();
}

#[test]
fn a_question_stays_quiet_until_the_answer_makes_it_claimable() {
    let (sandbox, log) = hooked_sandbox();
    let seq: i64 = sandbox
        .run(&["add", "choose compatibility"])
        .trim()
        .parse()
        .unwrap();
    wait_for(&log, &format!("filed {seq} choose compatibility"));

    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": seq})).unwrap();
    codex
        .call(
            "task_release",
            json!({
                "seq": seq,
                "reason": "implementation point isolated",
                "question": "Preserve the legacy format?"
            }),
        )
        .unwrap();
    let before = std::fs::read_to_string(&log).unwrap();
    assert!(
        !before.contains(&format!("released {seq}")),
        "a task awaiting a human must not summon another agent:\n{before}"
    );

    sandbox.run(&["answer", &seq.to_string(), "No; migrate it."]);
    wait_for(&log, &format!("answered {seq} choose compatibility"));
    codex.shutdown();
}

#[test]
fn without_a_hook_nothing_runs_and_nothing_breaks() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "quiet"]).trim().parse().unwrap();
    let listed = sandbox.run(&["ls"]);
    assert!(listed.contains("quiet"), "{listed}");
    assert_eq!(seq, 1);
}
