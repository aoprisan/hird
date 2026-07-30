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

    // Neither HIRD_HARNESS (removed by Sandbox::command) nor a client willing
    // to name itself. There is nothing left to go on, so the board says so.
    let mut session = McpSession::start_anonymous(&sandbox);

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"].as_str().unwrap().starts_with("unknown:"),
        "{}",
        claimed["holder"]
    );
    session.shutdown();
}

/// A harness configured by hand, without `hird register`, still lands on the
/// board under its own name: the client says who it is, and hird listens.
#[test]
fn a_harness_that_set_no_variable_is_named_by_its_client() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "t"]).trim().parse().unwrap();

    let mut session = McpSession::start_unnamed(&sandbox);

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    let holder = claimed["holder"].as_str().unwrap();
    assert!(
        holder.starts_with(&format!("{}:", support::CLIENT_NAME)),
        "{holder}"
    );
    session.shutdown();
}

/// `HIRD_HARNESS` is what the human wrote into their config; a client cannot
/// talk its way out of it by calling itself something else.
#[test]
fn the_configured_harness_name_outranks_the_clients_own() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "t"]).trim().parse().unwrap();

    let mut session = McpSession::start(&sandbox, "claude-code");

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"]
            .as_str()
            .unwrap()
            .starts_with("claude-code:"),
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
    // The headline says the work left a mark, and hedges it: the other agent
    // was live in that file too, so the count is not an account of who typed.
    let footprint = done["footprint"].as_str().unwrap_or_default();
    assert!(footprint.starts_with("modified 1 file"), "{done}");
    assert!(footprint.contains("another agent"), "{done}");

    // And the human's board says the same thing.
    let shown = sandbox.run(&["show", "1"]);
    assert!(shown.contains("changed   modified 1 file"), "{shown}");
    assert!(
        shown.contains("          src/config.rs (modified)"),
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

// ------------------------------------------------- the footing under a fact

/// The failure this feature exists for, end to end and over the wire.
///
/// One agent learns something about a file and writes it down. Somebody else
/// rewrites that file. A third agent picks up work in the same territory and is
/// handed the fact — and, because hird recorded what the fact was read off, is
/// told in the same breath that the ground under it has moved. Without this the
/// third agent gets a confident sentence about code that no longer exists, with
/// nothing in the payload to suggest it should look.
#[test]
fn a_fact_arrives_marked_shaky_once_its_file_has_been_rewritten() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() { env_first() }\n");
    sandbox.git_init();
    sandbox.run(&["add", "port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["add", "audit the config loader", "--path", "src/config.rs"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    let stored = codex
        .call(
            "mem_store",
            json!({
                "content": "the loader reads the env var before the file",
                "task_seq": 1,
            }),
        )
        .unwrap();
    assert_eq!(stored["anchored_to"], json!(["src/config.rs"]));
    codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();

    // While the fact is on file, the file it describes still says what it said.
    let firm = codex
        .call("mem_search", json!({"query": "loader"}))
        .unwrap();
    assert_eq!(firm["assertions"][0]["standing"], "firm");

    // Somebody rewrites it.
    sandbox.write_file("src/config.rs", "fn load() { file_first() }\n");

    let shaky = codex
        .call("mem_search", json!({"query": "loader"}))
        .unwrap();
    assert_eq!(shaky["assertions"][0]["standing"], "shaky");
    let footing = shaky["assertions"][0]["footing"]
        .as_str()
        .unwrap_or_default();
    assert!(footing.contains("src/config.rs"), "{footing}");
    assert!(footing.contains("re-read"), "{footing}");

    // And the next agent to work those files is told without having to ask.
    let mut claude = McpSession::start(&sandbox, "claude-code");
    let claim = claude
        .call("task_claim", json!({"seq": 2, "paths": ["src/config.rs"]}))
        .unwrap();
    let recalled = &claim["recalled"][0];
    assert_eq!(
        recalled["content"],
        "the loader reads the env var before the file"
    );
    assert_eq!(recalled["standing"], "shaky");
    assert!(
        recalled["caution"]
            .as_str()
            .unwrap_or_default()
            .contains("re-read"),
        "{recalled}"
    );

    codex.shutdown();
    claude.shutdown();
}

/// The way back. An agent that checks a shaky fact and finds it still true has
/// only one way to say so — say it again — and saying it again must not fork
/// the memory. It re-anchors the original and records a second voice, and
/// because the two agents are in different harnesses hird can say that.
#[test]
fn restating_a_shaky_fact_makes_it_firm_again_and_records_a_second_voice() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() { env_first() }\n");
    sandbox.git_init();
    sandbox.run(&["add", "port the loader", "--path", "src/config.rs"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    let first = codex
        .call(
            "mem_store",
            json!({"content": "env beats the config file", "task_seq": 1}),
        )
        .unwrap();
    let id = first["id"].as_str().unwrap().to_string();
    assert_eq!(first["affirmed"], json!(null), "nothing to affirm yet");
    codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();

    sandbox.write_file("src/config.rs", "fn load() { env_first(); /* tidied */ }\n");
    let shaky = codex.call("mem_search", json!({"query": "env"})).unwrap();
    assert_eq!(shaky["assertions"][0]["standing"], "shaky");

    // A different harness reads the file, finds the fact still holds, says so.
    let mut claude = McpSession::start(&sandbox, "claude-code");
    let again = claude
        .call(
            "mem_store",
            json!({"content": "env beats the config file", "paths": ["src/config.rs"]}),
        )
        .unwrap();
    assert_eq!(again["affirmed"], true);
    assert_eq!(again["id"], id, "one fact, not two");
    let voices = again["corroboration"].as_str().unwrap_or_default();
    assert!(
        voices.contains("independently across 2 harnesses"),
        "{voices}"
    );

    let firm = claude.call("mem_search", json!({"query": "env"})).unwrap();
    assert_eq!(firm["count"], 1);
    assert_eq!(firm["assertions"][0]["standing"], "firm");

    codex.shutdown();
    claude.shutdown();
}

/// A task that records a fact and then keeps editing the same file would, left
/// alone, mark its own fact shaky by its own hand. Finishing settles it against
/// the tree the task is leaving behind, so `shaky` keeps meaning "somebody else
/// moved this" — which is the only reading worth a warning.
#[test]
fn a_task_settles_its_own_facts_when_it_finishes() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() {}\n");
    sandbox.git_init();
    sandbox.run(&["add", "port the loader", "--path", "src/config.rs"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    codex
        .call(
            "mem_store",
            json!({"content": "the loader has one entry point", "task_seq": 1}),
        )
        .unwrap();
    // The agent carries on working, as agents do.
    sandbox.write_file("src/config.rs", "fn load() { /* now with a body */ }\n");
    let midway = codex
        .call("mem_search", json!({"query": "loader"}))
        .unwrap();
    assert_eq!(midway["assertions"][0]["standing"], "shaky");

    codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();
    let after = codex
        .call("mem_search", json!({"query": "loader"}))
        .unwrap();
    assert_eq!(
        after["assertions"][0]["standing"], "firm",
        "the task's own edits are not somebody else's drift"
    );

    codex.shutdown();
}

/// Outside git, and with the footing switched off, memory answers exactly as it
/// did before any of this existed — and the handshake does not promise a model
/// a `standing` field that nothing will ever set.
#[test]
fn memory_without_a_footing_says_nothing_about_standing() {
    for (label, setup) in [("no git", false), ("switched off", true)] {
        let sandbox = Sandbox::new();
        if setup {
            sandbox.git_init();
            sandbox.write_config("memory_footing = false\n");
        }
        sandbox.write_file("src/config.rs", "fn load() {}\n");
        sandbox.run(&["add", "port the loader", "--path", "src/config.rs"]);

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
            !instructions.contains("standing"),
            "{label}: a model must not be told about a field nothing will populate"
        );

        codex
            .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
            .unwrap();
        let stored = codex
            .call(
                "mem_store",
                json!({"content": "the loader has one entry point", "task_seq": 1}),
            )
            .unwrap();
        assert!(stored.get("anchored_to").is_none(), "{label}: {stored}");
        sandbox.write_file("src/config.rs", "fn load() { changed() }\n");
        let found = codex
            .call("mem_search", json!({"query": "loader"}))
            .unwrap();
        assert_eq!(found["count"], 1, "{label}");
        assert!(
            found["assertions"][0].get("standing").is_none(),
            "{label}: {found}"
        );

        codex.shutdown();
    }
}

// ------------------------------------------------- no agent reviews its own work

/// The whole point of running three models on one codebase, made to happen by
/// itself: work marked for review finishes, files its own review scoped to
/// exactly what moved, and the agent that wrote it is refused — not by
/// convention, by the queue, in the same transaction as the claim.
#[test]
fn work_marked_for_review_is_handed_to_a_different_harness() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() {}\n");
    sandbox.git_init();
    sandbox.run(&[
        "add",
        "Port the config loader",
        "--review",
        "--path",
        "src/config.rs",
    ]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex
        .call("task_claim", json!({"seq": 1, "paths": ["src/config.rs"]}))
        .unwrap();
    sandbox.write_file("src/config.rs", "fn load() { ported() }\n");
    let done = codex
        .call(
            "task_complete",
            json!({"seq": 1, "result": "ported it; env still wins"}),
        )
        .unwrap();
    let review = done["review_filed"].as_i64().expect("a review was filed");
    assert!(
        done["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("another harness"),
        "{done}"
    );

    // Codex cannot take it, and is told why in a sentence it can relay.
    let refused = codex
        .call("task_claim", json!({"seq": review}))
        .unwrap_err();
    assert!(refused.contains("codex"), "{refused}");
    assert!(refused.contains("a different harness"), "{refused}");

    // And the list says so too, rather than showing it as available work.
    let listed = codex.call("task_list", json!({"status": "open"})).unwrap();
    assert!(
        listed["tasks"][0]["recused_from_you"]
            .as_str()
            .unwrap_or_default()
            .contains("another harness"),
        "{listed}"
    );

    // Nor by asking for "whatever is workable" — dispatch routes around it
    // rather than handing out something it will then refuse.
    let next = codex.call("task_next", json!({})).unwrap();
    assert!(next.get("claimed").is_none(), "{next}");
    assert_eq!(next["recused"][0]["seq"], review);
    assert!(
        next["idle"]
            .as_str()
            .unwrap_or_default()
            .contains("another harness"),
        "{next}"
    );

    // Claude Code can, and arrives knowing what to read and what not to trust.
    let mut claude = McpSession::start(&sandbox, "claude-code");
    let claimed = claude.call("task_claim", json!({"seq": review})).unwrap();
    assert_eq!(claimed["title"], "Review: Port the config loader");
    assert_eq!(claimed["paths"], json!(["src/config.rs"]));
    let body = claimed["body"].as_str().unwrap_or_default();
    assert!(body.contains("ported it; env still wins"), "{body}");
    assert!(body.contains("src/config.rs (modified)"), "{body}");
    assert!(body.contains("not the summary"), "{body}");

    codex.shutdown();
    claude.shutdown();
}

/// A queue whose only remaining work is a review of your own code is not an
/// idle queue. Saying "nothing to do" would send the human away from the one
/// thing that needs them.
#[test]
fn a_recused_queue_says_it_needs_another_harness_rather_than_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() {}\n");
    sandbox.git_init();
    sandbox.run(&[
        "add",
        "Port the loader",
        "--review",
        "--path",
        "src/config.rs",
    ]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_next", json!({})).unwrap();
    sandbox.write_file("src/config.rs", "fn load() { ported() }\n");
    codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();

    let next = codex.call("task_next", json!({})).unwrap();
    let idle = next["idle"].as_str().unwrap_or_default();
    assert!(idle.contains("review of work this harness did"), "{idle}");
    assert!(idle.contains("waiting will not change it"), "{idle}");

    codex.shutdown();
}

/// Nothing is filed for work nobody asked to have reviewed, so a queue that
/// never uses the flag behaves exactly as it did before any of this existed.
#[test]
fn unreviewed_work_finishes_the_way_it_always_did() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() {}\n");
    sandbox.git_init();
    sandbox.run(&["add", "Port the loader", "--path", "src/config.rs"]);

    let mut codex = McpSession::start(&sandbox, "codex");
    codex.call("task_claim", json!({"seq": 1})).unwrap();
    sandbox.write_file("src/config.rs", "fn load() { ported() }\n");
    let done = codex
        .call("task_complete", json!({"seq": 1, "result": "ported"}))
        .unwrap();
    assert!(done.get("review_filed").is_none(), "{done}");
    assert_eq!(sandbox.run(&["ls"]).lines().count(), 1);

    codex.shutdown();
}

// ---------------------------------------------------------------------------
// MCP 2026-07-28: the stateless lifecycle.
//
// There is no `initialize` and no session to hold state in. A client asks the
// server to describe itself with `server/discover`, then sends requests that
// each carry the protocol version and its own name in `_meta`. hird is one
// process per session over stdio, so nothing about it depended on the
// handshake; these tests are here to keep it that way.
// ---------------------------------------------------------------------------

#[test]
fn discovery_answers_without_a_handshake_and_offers_the_current_spec() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start_stateless(&sandbox, Some("claude-code"));

    let response = session.request("server/discover", json!({}));
    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "hird");
    assert!(result["capabilities"]["tools"].is_object());

    let versions: Vec<&str> = result["supportedVersions"]
        .as_array()
        .expect("supportedVersions")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        versions.contains(&"2026-07-28"),
        "discovery does not offer the current spec: {versions:?}"
    );
    // The versions hird's own tests and docs assume still keep working.
    assert!(versions.contains(&"2025-11-25"), "{versions:?}");
    assert!(versions.contains(&"2025-06-18"), "{versions:?}");

    // The queue's rules reach a client that never calls `initialize`.
    let instructions = result["instructions"].as_str().expect("instructions");
    for expected in ["task_claim", "task_update", "mem_store", "seq"] {
        assert!(
            instructions.contains(expected),
            "instructions omit {expected}"
        );
    }

    // Those instructions name this project and are shaped by this machine's
    // config, so they are nobody else's to reuse.
    assert_eq!(result["cacheScope"], "private");
    assert_eq!(result["ttlMs"], 0);

    session.shutdown();
}

#[test]
fn stateless_tool_list_carries_required_cache_hints() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start_stateless(&sandbox, Some("opencode"));

    let response = session.request("tools/list", json!({}));
    let result = &response["result"];
    assert!(result["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty()));
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["cacheScope"], "private");
    assert_eq!(result["ttlMs"], 0);

    session.shutdown();
}

#[test]
fn a_stateless_client_can_work_a_task_end_to_end() {
    let sandbox = Sandbox::new();
    sandbox.write_file("src/config.rs", "fn load() {}\n");
    sandbox.git_init();
    let seq: i64 = sandbox
        .run(&["add", "Port the loader", "--path", "src/config.rs"])
        .trim()
        .parse()
        .unwrap();

    let mut session = McpSession::start_stateless(&sandbox, Some("claude-code"));

    let listed = session.call("task_list", json!({})).unwrap();
    assert_eq!(listed["count"], 1);

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"]
            .as_str()
            .unwrap()
            .starts_with("claude-code:"),
        "{}",
        claimed["holder"]
    );

    session
        .call(
            "task_scope",
            json!({"seq": seq, "paths": ["src/config.rs"]}),
        )
        .unwrap();
    session
        .call(
            "task_update",
            json!({"seq": seq, "note": "porting", "status": "in_progress"}),
        )
        .unwrap();
    sandbox.write_file("src/config.rs", "fn load() { ported() }\n");
    session
        .call("task_complete", json!({"seq": seq, "result": "ported"}))
        .unwrap();

    assert!(sandbox
        .run(&["ls", "--status", "done"])
        .contains("Port the loader"));
    session.shutdown();
}

/// The point of the stateless lifecycle for hird: `clientInfo` arrives on
/// every request, so a harness that never set `HIRD_HARNESS` is still somebody.
#[test]
fn a_stateless_client_names_itself_on_every_request() {
    let sandbox = Sandbox::new();
    let seq: i64 = sandbox.run(&["add", "t"]).trim().parse().unwrap();

    let mut session = McpSession::start_stateless_as(&sandbox, "codex-cli");

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"]
            .as_str()
            .unwrap()
            .starts_with("codex-cli:"),
        "{}",
        claimed["holder"]
    );
    session.shutdown();
}

/// Two harnesses on one queue is the whole design, and it has to survive one
/// of them being on the new lifecycle and the other on the old one.
#[test]
fn a_stateless_agent_and_a_handshaking_one_share_one_queue() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.run(&["add", "port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["add", "rewrite the renderer", "--path", "src/tui/**"]);

    let mut modern = McpSession::start_stateless(&sandbox, Some("claude-code"));
    let mut legacy = McpSession::start(&sandbox, "codex");

    let first = modern.call("task_next", json!({})).unwrap();
    let second = legacy.call("task_next", json!({})).unwrap();

    let a = first["claimed"]["claimed"]
        .as_i64()
        .expect("the stateless agent got work");
    let b = second["claimed"]["claimed"]
        .as_i64()
        .expect("the handshaking agent got work");
    assert_ne!(a, b, "both harnesses were handed task {a}");
    assert!(
        first["claimed"]["holder"]
            .as_str()
            .unwrap()
            .starts_with("claude-code:"),
        "{first}"
    );
    assert!(
        second["claimed"]["holder"]
            .as_str()
            .unwrap()
            .starts_with("codex:"),
        "{second}"
    );

    // And the loser of a race is told who won, across the lifecycle divide.
    let refused = legacy
        .call("task_claim", json!({"seq": a}))
        .expect_err("claiming a held task");
    assert!(refused.contains("claude-code:"), "{refused}");

    modern.shutdown();
    legacy.shutdown();
}

/// A request that cannot open a connection is answered rather than dropped.
///
/// The SDK's own response to an unopenable first message is to close the
/// transport, which reaches the harness as "the hird server crashed". It did
/// not crash, and a request with an id is owed an answer that says so.
#[test]
fn an_opening_request_that_cannot_open_anything_is_answered() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start_raw(&sandbox);

    // Declares the current spec but carries none of what it requires.
    let response = session.request_verbatim(
        "tools/list",
        json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}),
    );
    let error = response.get("error").unwrap_or_else(|| {
        panic!("a request missing its required metadata was served: {response}")
    });
    assert_eq!(error["code"], -32600);
    let message = error["message"].as_str().unwrap_or_default();
    for expected in ["initialize", "clientInfo", "_meta"] {
        assert!(message.contains(expected), "{message}");
    }

    // And hird stands down rather than serving a session it never opened,
    // saying the same thing to the human reading the harness's log.
    let stderr = session.wait_for_exit();
    assert!(stderr.contains("clientInfo"), "{stderr}");
}

/// A version hird does not implement is refused by name, rather than being
/// quietly served as something else.
#[test]
fn a_request_declaring_an_unknown_version_is_refused() {
    let sandbox = Sandbox::new();
    let mut session = McpSession::start_stateless(&sandbox, Some("claude-code"));

    // Open the connection properly first; this is about a later request.
    session.call("task_list", json!({})).unwrap();

    let response = session.request_verbatim(
        "tools/list",
        json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2099-01-01",
            "io.modelcontextprotocol/clientInfo": {"name": "from-the-future", "version": "0"},
            "io.modelcontextprotocol/clientCapabilities": {},
        }}),
    );
    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("an unknown protocol version was served: {response}"));
    assert_eq!(error["code"], -32022, "{error}");
    assert_eq!(error["data"]["requested"], "2099-01-01", "{error}");
    let supported: Vec<&str> = error["data"]["supported"]
        .as_array()
        .expect("the refusal names what is supported")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(supported.contains(&"2026-07-28"), "{supported:?}");

    // The connection survives one bad request: the next good one still works.
    let listed = session.call("task_list", json!({})).unwrap();
    assert_eq!(listed["count"], 0);

    session.shutdown();
}
