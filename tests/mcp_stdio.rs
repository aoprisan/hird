//! Drives `hird mcp` as a real subprocess, speaking JSON-RPC over its stdio.
//!
//! This is the contract a harness actually depends on, so the test does no
//! in-process shortcuts: it spawns the binary, initializes, lists tools and
//! calls them, exactly as Claude Code or Codex would.

mod support;

use serde_json::json;
use support::{McpSession, Sandbox};

#[test]
fn initialize_advertises_tools_and_the_queue_rules() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start(&sandbox, "claude-code");

    let response = session.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "integration-test", "version": "0"},
        }),
    );
    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "hird");
    assert!(result["capabilities"]["tools"].is_object());

    let instructions = result["instructions"].as_str().expect("instructions");
    for expected in ["task_claim", "task_update", "mem_store", "seq"] {
        assert!(
            instructions.contains(expected),
            "instructions omit {expected}"
        );
    }
    session.shutdown();
}

#[test]
fn tools_list_returns_exactly_the_designed_tools() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start(&sandbox, "claude-code");

    let response = session.request("tools/list", json!({}));
    let mut names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "mem_search",
            "mem_store",
            "task_claim",
            "task_complete",
            "task_fail",
            "task_get",
            "task_list",
            "task_next",
            "task_release",
            "task_scope",
            "task_split",
            "task_update",
        ]
    );

    // Each tool must carry a schema the harness can render.
    for tool in response["result"]["tools"].as_array().unwrap() {
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{} has no description",
            tool["name"]
        );
        assert!(
            tool["inputSchema"].is_object(),
            "{} has no schema",
            tool["name"]
        );
    }
    session.shutdown();
}

#[test]
fn an_agent_can_claim_work_it_through_and_complete_it() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox
        .run(&["add", "write the parser", "--body", "start with the lexer"])
        .trim()
        .parse()
        .expect("add prints the seq");

    let mut session = McpSession::start(&sandbox, "claude-code");

    let listed = session.call("task_list", json!({})).unwrap();
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["tasks"][0]["seq"], seq);

    let fetched = session.call("task_get", json!({"seq": seq})).unwrap();
    assert_eq!(fetched["body"], "start with the lexer");

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert_eq!(claimed["body"], "start with the lexer");
    let holder = claimed["holder"].as_str().unwrap().to_string();
    assert!(holder.starts_with("claude-code:"), "{holder}");

    let updated = session
        .call(
            "task_update",
            json!({"seq": seq, "status": "in_progress", "note": "lexer reads"}),
        )
        .unwrap();
    assert_eq!(updated["status"], "in_progress");

    let done = session
        .call(
            "task_complete",
            json!({"seq": seq, "result": "parser lands"}),
        )
        .unwrap();
    assert_eq!(done["status"], "done");

    // The CLI sees the same database.
    let shown = sandbox.run(&["show", &seq.to_string()]);
    assert!(shown.contains("status    done"), "{shown}");
    assert!(shown.contains("result: parser lands"), "{shown}");
    assert!(
        shown.contains(&holder),
        "history should name the agent:\n{shown}"
    );

    session.shutdown();
}

#[test]
fn two_harnesses_race_and_the_loser_is_told_who_won() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "contended"]).trim().parse().unwrap();

    let mut claude = McpSession::start(&sandbox, "claude-code");
    let mut codex = McpSession::start(&sandbox, "codex");

    let winner = claude.call("task_claim", json!({"seq": seq})).unwrap();
    let winner_id = winner["holder"].as_str().unwrap().to_string();

    let err = codex
        .call("task_claim", json!({"seq": seq}))
        .expect_err("the second claim must fail");
    assert!(
        err.contains(&winner_id),
        "loser was not told who holds it: {err}"
    );
    assert!(
        err.starts_with(&format!("task {seq} is claimed by")),
        "{err}"
    );

    // The loser also cannot drive the task it does not hold.
    let err = codex
        .call("task_complete", json!({"seq": seq, "result": "sneaky"}))
        .expect_err("a non-holder must not complete the task");
    assert!(err.contains("only the lease holder"), "{err}");

    claude.shutdown();
    codex.shutdown();
}

#[test]
fn memory_written_by_one_harness_is_searchable_by_another() {
    let sandbox = Sandbox::new();
    let mut claude = McpSession::start(&sandbox, "claude-code");
    let mut codex = McpSession::start(&sandbox, "codex");

    let stored = claude
        .call(
            "mem_store",
            json!({
                "content": "the lexer lives in src/lex.rs",
                "tags": "parser,code",
            }),
        )
        .unwrap();
    assert!(stored["actor"]
        .as_str()
        .unwrap()
        .starts_with("claude-code:"));

    let found = codex.call("mem_search", json!({"query": "lexer"})).unwrap();
    assert_eq!(found["count"], 1);
    assert_eq!(
        found["assertions"][0]["content"],
        "the lexer lives in src/lex.rs"
    );
    assert_eq!(found["assertions"][0]["tags"], json!(["parser", "code"]));

    // And the human sees it from the CLI.
    let listed = sandbox.run(&["mem", "search", "lexer"]);
    assert!(listed.contains("the lexer lives in src/lex.rs"), "{listed}");

    claude.shutdown();
    codex.shutdown();
}

/// The whole point of recall, end to end and across two harnesses: what Codex
/// learns while working a file is handed to Claude Code when it claims a
/// different task in the same file, without anyone searching for it.
#[test]
fn what_one_harness_learned_is_handed_to_the_next_agent_in_those_files() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "Port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["add", "Rewrite the loader tests", "--path", "tests/**"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": 1})).unwrap();
    codex
        .call(
            "mem_store",
            json!({
                "content": "the loader reads HIRD_DB before the config file",
                "task_seq": 1,
            }),
        )
        .unwrap();
    codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();
    codex.shutdown();

    // A different harness, a different task, and nobody called mem_search.
    let mut claude = McpSession::start(&sandbox, "claude-code");
    let claimed = claude
        .call("task_claim", json!({"seq": 2, "paths": ["src/*.rs"]}))
        .unwrap();
    let recalled = &claimed["recalled"][0];
    assert_eq!(
        recalled["content"],
        "the loader reads HIRD_DB before the config file"
    );
    assert_eq!(recalled["task_seq"], 1);
    assert!(recalled["actor"].as_str().unwrap().starts_with("codex:"));
    assert!(
        recalled["why"].as_str().unwrap().contains("src/config.rs"),
        "{recalled}"
    );
    claude.shutdown();

    // And the human can see exactly what their agents are being told.
    let brief = sandbox.run(&["recall", "2"]);
    assert!(
        brief.contains("the loader reads HIRD_DB before the config file"),
        "{brief}"
    );
    assert!(brief.contains("learned on task 1"), "{brief}");
}

#[test]
fn errors_come_back_as_sentences_rather_than_protocol_failures() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start(&sandbox, "claude-code");

    assert_eq!(
        session.call("task_get", json!({"seq": 99})).unwrap_err(),
        "task 99 not found"
    );
    let err = session
        .call("task_list", json!({"status": "blocked"}))
        .unwrap_err();
    assert!(err.contains("unknown status"), "{err}");

    session.shutdown();
}

/// Harnesses spawn a fresh `hird mcp` for every session, so cold start has to
/// stay cheap. The budget in the design notes is 50 ms from exec to a usable
/// server; the median of several runs is used so a busy machine cannot flake it.
///
/// Measured in a real repository, because that is the expensive case: deciding
/// whether the working tree can be watched costs a `git` subprocess, and it is
/// paid before the server answers `initialize`.
#[test]
fn mcp_mode_starts_well_inside_the_startup_budget() {
    const BUDGET_MS: u128 = 50;
    const RUNS: usize = 9;

    let sandbox = Sandbox::new();
    sandbox.git_init();
    // Pay the schema-creation cost once, outside the measurement.
    sandbox.run(&["add", "warm the database"]);

    let mut timings: Vec<u128> = (0..RUNS)
        .map(|_| {
            let started = std::time::Instant::now();
            let session = McpSession::start(&sandbox, "claude-code");
            let elapsed = started.elapsed().as_millis();
            session.shutdown();
            elapsed
        })
        .collect();
    timings.sort_unstable();

    let median = timings[RUNS / 2];
    assert!(
        median < BUDGET_MS,
        "spawn to initialized took a median of {median}ms, over the {BUDGET_MS}ms budget \
         (all runs: {timings:?})"
    );
}

#[test]
fn an_unknown_harness_still_gets_a_usable_identity() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "t"]).trim().parse().unwrap();

    // HIRD_HARNESS is removed by Sandbox::command; start without re-adding it.
    let mut session = McpSession::start_unnamed(&sandbox);

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"].as_str().unwrap().starts_with("unknown:"),
        "{}",
        claimed["holder"]
    );
    session.shutdown();
}

/// Two harnesses, one queue, no human dispatcher: the scenario the whole
/// design exists for. Each agent asks for work, is handed a different task,
/// and neither ends up in the other's files.
#[test]
fn two_harnesses_asking_for_work_spread_out_across_the_queue() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["add", "rewrite the renderer", "--path", "src/tui/**"]);

    let mut claude = McpSession::start(&sandbox, "claude-code");
    let mut codex = McpSession::start(&sandbox, "codex");

    let first = claude.call("task_next", json!({})).unwrap();
    let second = codex.call("task_next", json!({})).unwrap();

    let a = first["claimed"]["claimed"]
        .as_i64()
        .expect("claude got work");
    let b = second["claimed"]["claimed"]
        .as_i64()
        .expect("codex got work");
    assert_ne!(a, b, "both harnesses were handed the same task");
    // Disjoint file scopes, so neither claim reports an overlap.
    assert!(first["claimed"].get("overlaps").is_none(), "{first}");
    assert!(second["claimed"].get("overlaps").is_none(), "{second}");

    // A third request finds the queue drained rather than double-booking.
    let empty = claude.call("task_next", json!({})).unwrap();
    assert!(empty.get("claimed").is_none(), "{empty}");
    assert!(empty["idle"].as_str().unwrap().contains("queue is empty"));

    claude.shutdown();
    codex.shutdown();
}

/// The collision case: one agent is in `src/**`, the other is told about it
/// rather than silently editing over the top.
#[test]
fn an_agent_is_told_when_another_harness_is_already_in_its_files() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "wide refactor", "--path", "src/**"]);
    sandbox.run(&["add", "fix the db module"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    let mut claude = McpSession::start(&sandbox, "claude-code");

    let held = codex.call("task_next", json!({})).unwrap();
    assert_eq!(held["claimed"]["claimed"], 1);

    // The second task declared nothing up front, so it is handed over — and
    // the overlap surfaces the moment the agent says what it will touch.
    let mine = claude.call("task_next", json!({})).unwrap();
    assert_eq!(mine["claimed"]["claimed"], 2);
    let scoped = claude
        .call("task_scope", json!({"seq": 2, "paths": ["src/db.rs"]}))
        .unwrap();
    let overlap = scoped["overlaps"][0].as_str().expect("an overlap");
    assert!(overlap.contains("src/db.rs overlaps src/**"), "{overlap}");
    assert!(overlap.contains("codex:"), "{overlap}");
    assert!(scoped["advice"]
        .as_str()
        .unwrap()
        .contains("tell the human"));

    // And the human sees the same thing from the outside.
    let radar = sandbox.run(&["agents"]);
    assert!(
        radar.contains("src/db.rs also claimed by codex:"),
        "{radar}"
    );

    codex.shutdown();
    claude.shutdown();
}

/// An agent that finds a task too big turns it into a plan the rest of the
/// swarm can work, without a human touching the board.
#[test]
fn an_agent_can_split_a_task_into_work_for_the_others() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "migrate the storage layer"]);

    let mut claude = McpSession::start(&sandbox, "claude-code");
    claude.call("task_claim", json!({"seq": 1})).unwrap();
    let split = claude
        .call(
            "task_split",
            json!({
                "seq": 1,
                "subtasks": [
                    {"title": "write the migration", "paths": ["src/db.rs"]},
                    {"title": "port the repository", "paths": ["src/repo/**"]},
                ],
            }),
        )
        .unwrap();
    assert_eq!(split["status"], "open");
    assert_eq!(split["subtasks"].as_array().unwrap().len(), 2);

    // Two other harnesses take one piece each, in parallel.
    let mut codex = McpSession::start(&sandbox, "codex");
    let mut copilot = McpSession::start(&sandbox, "copilot");
    let a = codex.call("task_next", json!({})).unwrap()["claimed"]["claimed"]
        .as_i64()
        .expect("codex got a piece");
    let b = copilot.call("task_next", json!({})).unwrap()["claimed"]["claimed"]
        .as_i64()
        .expect("copilot got a piece");
    assert_ne!(a, b);
    assert!([2, 3].contains(&a) && [2, 3].contains(&b));

    // The parent is not workable until both pieces are done.
    let blocked = claude.call("task_claim", json!({"seq": 1})).unwrap_err();
    assert!(blocked.contains("task 1 is blocked by"), "{blocked}");

    codex
        .call("task_complete", json!({"seq": a, "result": "done"}))
        .unwrap();
    copilot
        .call("task_complete", json!({"seq": b, "result": "done"}))
        .unwrap();

    let freed = claude.call("task_next", json!({})).unwrap();
    assert_eq!(
        freed["claimed"]["claimed"], 1,
        "the parent is workable again"
    );

    claude.shutdown();
    codex.shutdown();
    copilot.shutdown();
}

/// Handing work back has to be cheaper than failing it, or agents will sit on
/// leases they cannot use.
#[test]
fn a_released_task_goes_straight_back_to_the_next_agent() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "deploy to staging"]);

    let mut claude = McpSession::start(&sandbox, "claude-code");
    claude.call("task_claim", json!({"seq": 1})).unwrap();
    let released = claude
        .call(
            "task_release",
            json!({"seq": 1, "reason": "no deploy credentials in this session"}),
        )
        .unwrap();
    assert_eq!(released["status"], "open");

    let mut codex = McpSession::start(&sandbox, "codex");
    let taken = codex.call("task_next", json!({})).unwrap();
    assert_eq!(taken["claimed"]["claimed"], 1);

    claude.shutdown();
    codex.shutdown();
}

// ------------------------------------------------------------- the witness

/// The failure the whole thing exists to catch, end to end and through the
/// real binary: two harnesses, one checkout, one file, no commit between them.
///
/// Neither agent can see the other's session, git has nothing to diff because
/// nothing has been committed, and both `task_complete` calls would otherwise
/// succeed with one of the two edits silently gone.
#[test]
fn two_harnesses_writing_one_declared_file_are_told_about_each_other() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.run(&["add", "port the config loader"]);
    sandbox.run(&["add", "audit the config loader"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    let mut claude = McpSession::start(&sandbox, "claude-code");

    // Both declare the same file. That much the old collision detector saw.
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    let second = claude
        .call("task_claim", json!({"seq": 2, "paths": ["src/*.rs"]}))
        .unwrap();
    let overlaps = second["overlaps"].as_array().expect("overlaps");
    assert_eq!(overlaps.len(), 1, "the declared overlap is reported first");

    // Now the file actually moves. This is the part nothing else can see.
    sandbox.write_file("src/config.rs", "// codex ported it\n");
    let codex_update = codex
        .call("task_update", json!({"seq": 1, "note": "loader ported"}))
        .unwrap();
    assert_eq!(
        codex_update["changed"],
        json!(["src/config.rs (modified)"]),
        "the witness reports what moved, not what was promised"
    );

    sandbox.write_file("src/config.rs", "// claude rewrote it\n");
    let claude_update = claude
        .call("task_update", json!({"seq": 2, "note": "loader audited"}))
        .unwrap();
    assert_eq!(
        claude_update["changed"],
        json!(["src/config.rs (modified)"])
    );

    // And codex, on its next heartbeat, is told its copy is out of date
    // before it writes from it.
    let warned = codex
        .call("task_update", json!({"seq": 1, "note": "carrying on"}))
        .unwrap();
    let contended = warned["contended"]
        .as_array()
        .unwrap_or_else(|| panic!("codex was not warned: {warned}"));
    assert_eq!(contended.len(), 1, "{warned}");
    let sentence = contended[0].as_str().unwrap();
    assert!(sentence.contains("src/config.rs"), "{sentence}");
    assert!(sentence.contains("claude-code"), "{sentence}");
    assert!(sentence.contains("re-read"), "{sentence}");
    assert!(
        warned["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("re-read"),
        "{warned}"
    );

    // Completing carries the evidence, which was not written by the agent
    // that wrote the result line next to it.
    let done = codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();
    assert_eq!(done["changed"], json!(["src/config.rs (modified)"]));

    // And the human's board says the same thing.
    let shown = sandbox.run(&["show", "1"]);
    assert!(
        shown.contains("changed   src/config.rs (modified)"),
        "{shown}"
    );

    codex.shutdown();
    claude.shutdown();
}

/// Agents that stay in their own lanes must never hear about each other, or
/// the warning becomes noise and gets ignored the one time it matters.
#[test]
fn agents_in_separate_files_are_never_warned() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.run(&["add", "port the loader"]);
    sandbox.run(&["add", "write the docs"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    let mut claude = McpSession::start(&sandbox, "claude-code");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    claude
        .call("task_claim", json!({"seq": 2, "paths": ["README.md"]}))
        .unwrap();

    sandbox.write_file("src/config.rs", "// ported\n");
    sandbox.write_file("README.md", "# documented\n");
    let a = codex
        .call("task_update", json!({"seq": 1, "note": "done"}))
        .unwrap();
    let b = claude
        .call("task_update", json!({"seq": 2, "note": "done"}))
        .unwrap();
    assert!(a.get("contended").is_none(), "{a}");
    assert!(b.get("contended").is_none(), "{b}");

    codex.shutdown();
    claude.shutdown();
}

/// An agent that wanders outside what it declared is told so, because every
/// other agent's collision check is reading the declaration and not the edits.
#[test]
fn editing_outside_the_declared_scope_is_reported_back() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.run(&["add", "port the config loader"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    sandbox.write_file("src/config.rs", "// ported\n");
    sandbox.write_file("src/surprise.rs", "// and this too\n");

    let update = codex
        .call("task_update", json!({"seq": 1, "note": "ported"}))
        .unwrap();
    assert_eq!(update["undeclared"], json!(["src/surprise.rs"]));
    let advice = update["advice"].as_str().unwrap_or_default();
    assert!(advice.contains("task_scope"), "{advice}");

    // And declaring it makes the complaint go away rather than needing a flag.
    let scoped = codex
        .call(
            "task_scope",
            json!({"seq": 1, "paths": ["src/surprise.rs"]}),
        )
        .unwrap();
    assert!(scoped.get("undeclared").is_none(), "{scoped}");

    codex.shutdown();
}

/// A project outside git is not a degraded project. Every tool answers exactly
/// as it did before the witness existed, and the handshake does not promise
/// the model a field it will never see.
#[test]
fn a_project_without_git_behaves_as_though_the_witness_did_not_exist() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "do the thing"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    let instructions = codex.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "integration-test", "version": "0"},
        }),
    )["result"]["instructions"]
        .as_str()
        .expect("instructions")
        .to_string();
    assert!(
        !instructions.contains("contended"),
        "a model must not be told about a field nothing will populate"
    );

    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    sandbox.write_file("src/config.rs", "// edited anyway\n");
    let update = codex
        .call("task_update", json!({"seq": 1, "note": "working"}))
        .unwrap();
    assert!(update.get("changed").is_none(), "{update}");
    let done = codex
        .call("task_complete", json!({"seq": 1, "result": "done"}))
        .unwrap();
    assert_eq!(done["status"], "done");

    codex.shutdown();
}

/// Turning the witness off in the configuration has to be as complete as not
/// having git at all — including not shelling out to it.
#[test]
fn the_witness_can_be_switched_off_in_the_configuration() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.write_config("witness = false\n");
    sandbox.run(&["add", "do the thing"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    sandbox.write_file("src/config.rs", "// edited\n");
    let update = codex
        .call("task_update", json!({"seq": 1, "note": "working"}))
        .unwrap();
    assert!(update.get("changed").is_none(), "{update}");

    codex.shutdown();
}
