//! The question hook, end to end: a configured command hears about every
//! question gate the moment it opens.
//!
//! The dispatch hook stays deliberately quiet when a task releases with a
//! question — the task is waiting for a human, not another agent. The
//! question hook is that silence's twin: same detached contract, opposite
//! audience. The hook here appends one line per raised question to a log
//! file; what a real one does with it — a desktop notification being the case
//! the feature exists for — is not hird's business.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;
use support::{McpSession, Sandbox};

/// A sandbox whose config carries a question hook, and the log it writes.
fn hooked_sandbox() -> (Sandbox, PathBuf) {
    let sandbox = Sandbox::new();
    let log = sandbox.dir.path().join("questions.log");
    sandbox.write_config(&format!(
        "question_hook = \"echo \\\"$HIRD_EVENT $HIRD_TASK $HIRD_TITLE: $HIRD_QUESTION ($HIRD_ASKED_BY)\\\" >> {}\"\n",
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
fn a_raised_question_summons_the_human_with_the_question_itself() {
    let (sandbox, log) = hooked_sandbox();
    let seq: i64 = sandbox
        .run(&["add", "migrate the config"])
        .trim()
        .parse()
        .unwrap();

    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": seq})).unwrap();
    codex
        .call(
            "task_release",
            json!({
                "seq": seq,
                "reason": "the migration branch is isolated",
                "question": "Must the old format remain readable?"
            }),
        )
        .unwrap();

    // The whole announcement in one line: the event word, the task, the
    // question, and which agent hit the human edge.
    wait_for(
        &log,
        &format!("asked {seq} migrate the config: Must the old format remain readable? (codex:"),
    );
    codex.shutdown();
}

#[test]
fn an_ordinary_release_stays_quiet() {
    let (sandbox, log) = hooked_sandbox();
    let seq: i64 = sandbox.run(&["add", "contended"]).trim().parse().unwrap();

    // A release without a question raises no gate: the task goes back to the
    // pool, which is the dispatch hook's news, not this one's.
    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": seq})).unwrap();
    codex
        .call(
            "task_release",
            json!({"seq": seq, "reason": "out of context"}),
        )
        .unwrap();

    // A second round that does ask orders the log: once its line has landed,
    // the quiet release before it has had every chance to speak.
    codex.call("task_claim", json!({"seq": seq})).unwrap();
    codex
        .call(
            "task_release",
            json!({"seq": seq, "reason": "needs a policy call", "question": "Which way?"}),
        )
        .unwrap();
    let contents = wait_for(&log, &format!("asked {seq} contended: Which way?"));
    assert_eq!(
        contents.lines().count(),
        1,
        "the ordinary release must not have fired the hook:\n{contents}"
    );
    codex.shutdown();
}
