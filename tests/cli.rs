//! End-to-end tests for the command line, driving the real binary.

mod support;

use support::{assert_exists, McpSession, Sandbox};

#[test]
fn add_prints_the_task_number_and_numbers_climb() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "first"]).trim(), "1");
    assert_eq!(sandbox.run(&["add", "second"]).trim(), "2");
    assert_eq!(sandbox.run(&["add", "third"]).trim(), "3");
}

#[test]
fn db_path_reports_the_file_and_creating_a_task_makes_it() {
    let sandbox = Sandbox::new();
    let reported = sandbox.run(&["db-path"]);
    assert_eq!(reported.trim(), sandbox.db().to_string_lossy());
    // db-path must not create anything on its own.
    assert!(!sandbox.db().exists());

    sandbox.run(&["add", "t"]);
    assert_exists(&sandbox.db());
}

#[test]
fn an_explicit_db_flag_beats_the_environment() {
    let sandbox = Sandbox::new();
    let other = sandbox.dir.path().join("other.db");
    let reported = sandbox.run(&["--db", other.to_str().unwrap(), "db-path"]);
    assert_eq!(reported.trim(), other.to_string_lossy());

    sandbox.run(&["--db", other.to_str().unwrap(), "add", "elsewhere"]);
    assert_exists(&other);
    assert!(
        !sandbox.db().exists(),
        "the HIRD_DB database was not touched"
    );
}

#[test]
fn ls_shows_status_priority_and_nothing_else() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "plain task"]);
    sandbox.run(&["add", "urgent task", "--priority", "5"]);

    let listed = sandbox.run(&["ls"]);
    assert!(listed.contains("#1"), "{listed}");
    assert!(listed.contains("open"), "{listed}");
    assert!(listed.contains("urgent task"), "{listed}");
    assert!(listed.contains("p5"), "{listed}");
    // Priority 0 is the default and is not worth a column.
    let plain = listed.lines().find(|l| l.contains("plain task")).unwrap();
    assert!(!plain.contains("p0"), "{plain}");
}

#[test]
fn ls_filters_by_status_and_says_so_when_empty() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "a task"]);
    assert!(sandbox.run(&["ls", "--status", "open"]).contains("a task"));
    assert_eq!(sandbox.run(&["ls", "--status", "done"]).trim(), "no tasks");
}

#[test]
fn ls_rejects_an_unknown_status_with_the_valid_set() {
    let sandbox = Sandbox::new();
    let err = sandbox.run_failing(&["ls", "--status", "blocked"]);
    assert!(err.contains("unknown status"), "{err}");
    assert!(err.contains("in_progress"), "{err}");
}

#[test]
fn show_prints_the_body_history_and_linked_assertions() {
    let sandbox = Sandbox::new();
    let seq = sandbox
        .run(&["add", "write the parser", "--body", "start with the lexer"])
        .trim()
        .to_string();
    sandbox.run(&["mem", "add", "the lexer is hand written", "--task", &seq]);

    let shown = sandbox.run(&["show", &seq]);
    assert!(shown.contains("#1 write the parser"), "{shown}");
    assert!(shown.contains("status    open"), "{shown}");
    assert!(shown.contains("start with the lexer"), "{shown}");
    assert!(shown.contains("history"), "{shown}");
    assert!(shown.contains("created"), "{shown}");
    assert!(shown.contains("the lexer is hand written"), "{shown}");
}

#[test]
fn show_on_a_missing_task_fails_with_a_plain_sentence() {
    let sandbox = Sandbox::new();
    let err = sandbox.run_failing(&["show", "42"]);
    assert!(err.contains("task 42 not found"), "{err}");
}

#[test]
fn a_body_can_come_from_a_file() {
    let sandbox = Sandbox::new();
    let body = sandbox.dir.path().join("body.md");
    std::fs::write(&body, "# Instructions\n\nDo the thing.\n").unwrap();

    let seq = sandbox
        .run(&["add", "from a file", "--body-file", body.to_str().unwrap()])
        .trim()
        .to_string();
    assert!(sandbox.run(&["show", &seq]).contains("Do the thing."));
}

#[test]
fn cancel_and_reopen_move_the_task_and_are_recorded() {
    let sandbox = Sandbox::new();
    let seq = sandbox.run(&["add", "t"]).trim().to_string();

    assert_eq!(sandbox.run(&["cancel", &seq]).trim(), "task 1 cancelled");
    assert!(sandbox.run(&["show", &seq]).contains("status    cancelled"));

    assert_eq!(sandbox.run(&["reopen", &seq]).trim(), "task 1 reopened");
    let shown = sandbox.run(&["show", &seq]);
    assert!(shown.contains("status    open"), "{shown}");
    // Both human actions are attributed to the CLI in the history.
    assert!(shown.contains("cancelled     cli"), "{shown}");
    assert!(shown.contains("reopened      cli"), "{shown}");
}

#[test]
fn illegal_transitions_are_refused_with_the_current_status() {
    let sandbox = Sandbox::new();
    let seq = sandbox.run(&["add", "t"]).trim().to_string();
    let err = sandbox.run_failing(&["reopen", &seq]);
    assert!(err.contains("cannot reopen task 1: it is open"), "{err}");
}

#[test]
fn mem_add_prints_an_id_and_search_finds_it() {
    let sandbox = Sandbox::new();
    let id = sandbox
        .run(&[
            "mem",
            "add",
            "migrations live in src/db.rs",
            "--tags",
            "schema,code",
        ])
        .trim()
        .to_string();
    assert!(!id.is_empty());

    let found = sandbox.run(&["mem", "search", "migrations"]);
    assert!(found.contains("migrations live in src/db.rs"), "{found}");
    assert!(found.contains(&id), "{found}");
    assert!(found.contains("#schema #code"), "{found}");
    assert!(found.contains("cli"), "provenance is missing:\n{found}");
}

#[test]
fn mem_search_with_no_query_lists_everything_recent_first() {
    let sandbox = Sandbox::new();
    sandbox.run(&["mem", "add", "older fact"]);
    sandbox.run(&["mem", "add", "newer fact"]);

    let listed = sandbox.run(&["mem", "search"]);
    let newer = listed.find("newer fact").expect("newer listed");
    let older = listed.find("older fact").expect("older listed");
    assert!(newer < older, "newest should come first:\n{listed}");
}

#[test]
fn mem_search_says_so_when_nothing_matches() {
    let sandbox = Sandbox::new();
    sandbox.run(&["mem", "add", "a fact"]);
    assert_eq!(
        sandbox.run(&["mem", "search", "nonexistent"]).trim(),
        "no assertions"
    );
}

#[test]
fn project_scope_separates_two_checkouts() {
    let sandbox = Sandbox::new();
    let other = sandbox.dir.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();

    sandbox.run(&["add", "task here"]);
    let out = sandbox
        .command()
        .env("HIRD_PROJECT", &other)
        .args(["add", "task there"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Each project sees only its own task...
    assert!(!sandbox.run(&["ls"]).contains("task there"));
    // ...until asked for everything.
    let all = sandbox.run(&["ls", "--all-projects"]);
    assert!(all.contains("task here"), "{all}");
    assert!(all.contains("task there"), "{all}");
}

#[test]
fn the_project_flag_overrides_detection_for_a_single_task() {
    let sandbox = Sandbox::new();
    let other = sandbox.dir.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();

    sandbox.run(&[
        "add",
        "filed elsewhere",
        "--project",
        other.to_str().unwrap(),
    ]);
    assert_eq!(sandbox.run(&["ls"]).trim(), "no tasks");
    assert!(sandbox
        .run(&["ls", "--all-projects"])
        .contains("filed elsewhere"));
}

#[test]
fn the_config_file_changes_the_lease_ttl_the_server_advertises() {
    let sandbox = Sandbox::new();
    sandbox.write_config("lease_ttl_minutes = 40\n");

    // The TTL reaches the MCP instructions, which is where an agent reads it.
    let seq = sandbox.run(&["add", "t"]).trim().to_string();
    let shown = sandbox.run(&["show", &seq]);
    assert!(shown.contains("status    open"), "{shown}");

    let err = sandbox.run_failing(&["--db", "/", "ls"]);
    assert!(!err.is_empty(), "opening an impossible database must fail");
}

#[test]
fn a_broken_config_file_is_reported_rather_than_ignored() {
    let sandbox = Sandbox::new();
    sandbox.write_config("lease_ttl_minutes = \"forever\"\n");
    let err = sandbox.run_failing(&["ls"]);
    assert!(err.contains("lease_ttl_minutes"), "{err}");
}

#[test]
fn help_and_version_work_without_a_database() {
    let sandbox = Sandbox::new();
    let help = sandbox.run(&["--help"]);
    for expected in ["add", "ls", "show", "cancel", "reopen", "mem", "tui", "mcp"] {
        assert!(help.contains(expected), "help omits {expected}:\n{help}");
    }
    assert!(sandbox.run(&["--version"]).contains("hird"));
    assert!(!sandbox.db().exists());
}

#[test]
fn install_skill_writes_the_bundled_global_skill_without_opening_the_database() {
    let sandbox = Sandbox::new();
    let installed = sandbox.run(&["--install-skill"]);
    for relative in [
        ".agents/skills/hird/SKILL.md",
        ".claude/skills/hird/SKILL.md",
        ".copilot/skills/hird/SKILL.md",
    ] {
        let path = sandbox.dir.path().join(relative);
        assert!(
            installed.contains(&path.to_string_lossy().to_string()),
            "{installed}"
        );
        let skill = std::fs::read_to_string(path).unwrap();
        assert!(skill.contains("name: hird"), "{skill}");
        assert!(skill.contains("task_claim"), "{skill}");
    }
    assert!(!sandbox.db().exists());

    let repeated = sandbox.run(&["--install-skill"]);
    assert!(repeated.contains("already installed"), "{repeated}");
}

#[test]
fn install_copies_the_running_binary_without_opening_the_database() {
    let sandbox = Sandbox::new();
    let installed = sandbox.run(&["--install"]);
    let path = sandbox.dir.path().join(".local/bin/hird");

    assert!(
        installed.contains(&path.to_string_lossy().to_string()),
        "{installed}"
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        std::fs::read(support::bin()).unwrap()
    );
    assert!(!sandbox.db().exists());
}

#[test]
fn installer_options_refuse_normal_commands_before_doing_anything() {
    let sandbox = Sandbox::new();
    let err = sandbox.run_failing(&["--install-skill", "ls"]);
    assert!(err.contains("installer options cannot be used"), "{err}");
    assert!(!sandbox
        .dir
        .path()
        .join(".agents/skills/hird/SKILL.md")
        .exists());
    assert!(!sandbox
        .dir
        .path()
        .join(".claude/skills/hird/SKILL.md")
        .exists());
    assert!(!sandbox
        .dir
        .path()
        .join(".copilot/skills/hird/SKILL.md")
        .exists());
    assert!(!sandbox.db().exists());
}

#[test]
fn a_plan_can_be_filed_in_one_go_and_read_back_as_waves() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "write the schema", "--path", "src/db.rs"]);
    sandbox.run(&[
        "add",
        "write the api",
        "--needs",
        "1",
        "--path",
        "src/api.rs",
    ]);
    sandbox.run(&["add", "write the docs", "--needs", "1,2"]);

    let graph = sandbox.run(&["graph"]);
    assert!(graph.contains("wave 1  (workable now)"), "{graph}");
    // Each task lands one wave behind the last thing it waits for.
    let wave_of = |seq: &str| {
        graph
            .lines()
            .rev()
            .find(|l| l.contains(&format!("#{seq}")))
            .map(|_| {
                graph
                    .lines()
                    .take_while(|l| !l.contains(&format!("#{seq}")))
                    .filter(|l| l.starts_with("wave"))
                    .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(wave_of("1"), 1, "{graph}");
    assert_eq!(wave_of("2"), 2, "{graph}");
    assert_eq!(wave_of("3"), 3, "{graph}");

    let listed = sandbox.run(&["ls"]);
    assert!(listed.contains("waits #1"), "{listed}");
    assert!(listed.contains("waits #1,#2"), "{listed}");
}

#[test]
fn a_cycle_is_refused_with_the_chain_that_would_have_closed_it() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "a"]);
    sandbox.run(&["add", "b"]);
    sandbox.run(&["dep", "add", "2", "--needs", "1"]);

    let err = sandbox.run_failing(&["dep", "add", "1", "--needs", "2"]);
    assert!(err.contains("that would be a cycle"), "{err}");
    assert!(err.contains("2 -> 1"), "{err}");
}

#[test]
fn show_reports_what_a_task_waits_for_and_what_it_touches() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "write the schema"]);
    sandbox.run(&[
        "add",
        "write the api",
        "--needs",
        "1",
        "--path",
        "src/api.rs",
    ]);

    let shown = sandbox.run(&["show", "2"]);
    assert!(shown.contains("waits for #1 (open)"), "{shown}");
    assert!(shown.contains("files     src/api.rs"), "{shown}");

    let upstream = sandbox.run(&["show", "1"]);
    assert!(upstream.contains("blocks    #2"), "{upstream}");
}

#[test]
fn recall_answers_with_what_earlier_work_in_the_same_files_learned() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "Port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["mem", "add", "env vars beat the config file", "--task", "1"]);
    sandbox.run(&["add", "Audit the loader", "--path", "src/*.rs"]);

    let recalled = sandbox.run(&["recall", "2"]);
    assert!(
        recalled.contains("env vars beat the config file"),
        "{recalled}"
    );
    assert!(recalled.contains("learned on task 1"), "{recalled}");
    assert!(recalled.contains("src/config.rs"), "{recalled}");

    // `show` carries the same thing, under its own heading.
    let shown = sandbox.run(&["show", "2"]);
    assert!(shown.contains("recalled from earlier work"), "{shown}");
    assert!(shown.contains("env vars beat the config file"), "{shown}");
}

#[test]
fn recall_says_so_when_a_task_stands_alone() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "Xyzzy", "--path", "src/xyzzy.rs"]);
    assert!(sandbox
        .run(&["recall", "1"])
        .contains("nothing recorded so far touches task 1"));
    // And `show` stays quiet rather than printing an empty heading.
    assert!(!sandbox.run(&["show", "1"]).contains("recalled"));
}

#[test]
fn recall_can_be_turned_off_in_the_config_file() {
    let sandbox = Sandbox::new();
    sandbox.write_config("recall_limit = 0\n");
    sandbox.run(&["add", "Port the config loader", "--path", "src/config.rs"]);
    sandbox.run(&["mem", "add", "env vars beat the config file", "--task", "1"]);
    sandbox.run(&["add", "Audit the loader", "--path", "src/config.rs"]);

    assert!(sandbox
        .run(&["recall", "2"])
        .contains("nothing recorded so far touches task 2"));
    // The limit is a default, not a ceiling: asking explicitly still answers.
    assert!(sandbox
        .run(&["recall", "2", "--limit", "5"])
        .contains("env vars beat the config file"));
}

#[test]
fn a_scope_can_be_set_inspected_and_cleared() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "refactor"]);

    let set = sandbox.run(&["scope", "1", "--path", "./src/tui/", "--path", "README.md"]);
    assert_eq!(set, "src/tui/**\nREADME.md\n");
    assert_eq!(sandbox.run(&["scope", "1"]), "src/tui/**\nREADME.md\n");

    sandbox.run(&["scope", "1", "--clear"]);
    assert!(sandbox.run(&["scope", "1"]).contains("declares no files"));
}

#[test]
fn a_path_that_climbs_out_of_the_project_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "t"]);
    let err = sandbox.run_failing(&["scope", "1", "--path", "../../etc/passwd"]);
    assert!(err.contains("not a usable path pattern"), "{err}");
}

#[test]
fn the_agents_view_is_quiet_when_nobody_is_working() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "t"]);
    assert!(sandbox
        .run(&["agents"])
        .contains("no agent is working anything right now"));
}

/// The human's view of the witness: what each agent said, what actually moved,
/// and the file two of them are both standing in.
///
/// Driven entirely through the CLI, which is the only front end that can set
/// this up without an agent session — `hird agents` sweeps the tree itself.
#[test]
fn the_agents_view_shows_what_moved_and_where_two_agents_meet() {
    let sandbox = Sandbox::new();
    sandbox.git_init();
    sandbox.run(&["add", "port the loader", "--path", "src/config.rs"]);
    sandbox.run(&["add", "audit the loader", "--path", "src/*.rs"]);

    // Two live claims, made the way the MCP server makes them.
    let mut codex = McpSession::start(&sandbox, "codex");
    let mut claude = McpSession::start(&sandbox, "claude-code");
    codex.claim(1);
    claude.claim(2);

    // Codex edits and checks in, so hird has shown it that version.
    sandbox.write_file("src/config.rs", "// codex ported it\n");
    codex
        .call(
            "task_update",
            serde_json::json!({"seq": 1, "note": "ported"}),
        )
        .unwrap();

    let board = sandbox.run(&["agents"]);
    assert!(board.contains("files  src/config.rs"), "{board}");
    assert!(board.contains("moved  src/config.rs"), "{board}");
    assert!(
        !board.contains("!!!"),
        "both agents are level on the file, so there is nothing to warn about:\n{board}"
    );

    // Now claude writes over it and checks in. Codex's copy is the old one.
    sandbox.write_file("src/config.rs", "// claude rewrote it\n");
    claude
        .call(
            "task_update",
            serde_json::json!({"seq": 2, "note": "audited"}),
        )
        .unwrap();

    let board = sandbox.run(&["agents"]);
    assert!(
        board.contains("!!!") && board.contains("re-read"),
        "the human's board must name the agent holding the stale copy:\n{board}"
    );

    codex.shutdown();
    claude.shutdown();
}

/// A project that is not a git repository must print exactly what it always
/// printed. Nothing about the board may depend on the witness being able to
/// look.
#[test]
fn the_agents_view_outside_git_says_nothing_about_the_tree() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "port the loader", "--path", "src/config.rs"]);
    let mut codex = McpSession::start(&sandbox, "codex");
    codex.claim(1);

    let board = sandbox.run(&["agents"]);
    assert!(board.contains("files  src/config.rs"), "{board}");
    assert!(!board.contains("moved"), "{board}");

    codex.shutdown();
}

// ------------------------------------------------------------------- plans

/// The plan used by the tests below: five tasks, three waves, one unordered
/// pair of tasks that declare the same files.
const PLAN: &str = r#"
plan = "serde-migration"

[[task]]
name = "schema"
title = "Design the storage schema"
priority = 3
paths = ["src/db.rs"]

[[task]]
name = "repos"
title = "Port the repository layer"
body = "Keep the env-var precedence."
paths = ["src/repo/**"]
needs = ["schema"]

[[task]]
name = "renderer"
title = "Rewrite the renderer"
paths = ["src/tui/**"]

[[task]]
name = "audit"
title = "Audit the renderer tests"
paths = ["src/tui/view.rs"]

[[task]]
name = "notes"
title = "Write the release notes"
needs = ["repos", "renderer"]
"#;

#[test]
fn a_plan_files_its_whole_graph_and_the_board_agrees_with_the_preview() {
    let sandbox = Sandbox::new();
    sandbox.write_file("plan.toml", PLAN);

    let preview = sandbox.run(&["plan", "apply", "plan.toml", "--dry-run"]);
    assert!(preview.contains("5 tasks, 3 waves"), "{preview}");
    assert!(preview.contains("nothing was written"), "{preview}");
    // Nothing was filed, so there is nothing to list.
    assert!(sandbox.run(&["ls"]).contains("no tasks"), "still empty");

    let filed = sandbox.run(&["plan", "apply", "plan.toml"]);
    assert!(filed.contains("filed 5 tasks"), "{filed}");
    assert!(filed.contains("3 dependencies recorded"), "{filed}");

    // The waves the preview promised are the waves the queue reports.
    let graph = sandbox.run(&["graph"]);
    for (wave, title) in [
        ("wave 1", "Design the storage schema"),
        ("wave 2", "Port the repository layer"),
        ("wave 3", "Write the release notes"),
    ] {
        let at = graph.find(wave).unwrap_or_else(|| panic!("{graph}"));
        let next = graph[at..].find(title);
        assert!(next.is_some(), "{title} should follow {wave}:\n{graph}");
    }
    assert!(sandbox.run(&["show", "2"]).contains("waits for #1"));
    assert!(sandbox.run(&["scope", "2"]).contains("src/repo/**"));
}

#[test]
fn a_plan_applied_twice_files_only_what_the_file_gained() {
    let sandbox = Sandbox::new();
    sandbox.write_file("plan.toml", PLAN);
    sandbox.run(&["plan", "apply", "plan.toml"]);

    let again = sandbox.run(&["plan", "apply", "plan.toml"]);
    assert!(again.contains("already filed in full"), "{again}");
    assert_eq!(sandbox.run(&["ls"]).lines().count(), 5);

    sandbox.write_file(
        "plan.toml",
        &format!(
            "{PLAN}
[[task]]
name = \"changelog\"
title = \"Update the changelog\"
needs = [\"notes\"]
"
        ),
    );
    let grown = sandbox.run(&["plan", "apply", "plan.toml"]);
    assert!(grown.contains("filed 1 task"), "{grown}");
    assert!(grown.contains("#6"), "{grown}");
    assert!(grown.contains("5 tasks already filed"), "{grown}");
    assert_eq!(sandbox.run(&["ls"]).lines().count(), 6);
}

#[test]
fn the_preview_names_the_pairs_that_cannot_run_at_once() {
    let sandbox = Sandbox::new();
    sandbox.write_file("plan.toml", PLAN);

    let preview = sandbox.run(&["plan", "apply", "plan.toml", "--dry-run"]);
    assert!(
        preview.contains("renderer and audit — src/tui/** overlaps src/tui/view.rs"),
        "{preview}"
    );
    // And the task that told the queue nothing about its files.
    assert!(preview.contains("declaring no files: notes"), "{preview}");
}

#[test]
fn the_preview_marks_what_is_already_filed() {
    let sandbox = Sandbox::new();
    sandbox.write_file("plan.toml", PLAN);
    sandbox.run(&["plan", "apply", "plan.toml"]);

    let preview = sandbox.run(&["plan", "apply", "plan.toml", "--dry-run"]);
    assert!(preview.contains("#1    schema"), "{preview}");
    assert!(!preview.contains("new   schema"), "{preview}");
    assert!(
        preview.contains("5 of these already filed, so applying would file the other 0"),
        "{preview}"
    );
}

#[test]
fn a_plan_that_cannot_be_filed_leaves_the_queue_untouched() {
    let sandbox = Sandbox::new();
    sandbox.run(&["add", "filed by hand"]);

    // A name nothing defines, with a near-miss to point at.
    sandbox.write_file(
        "plan.toml",
        r#"
plan = "p"
[[task]]
name = "schema"
title = "Design the schema"
[[task]]
name = "repos"
title = "Port the repositories"
needs = ["schemas"]
"#,
    );
    let err = sandbox.run_failing(&["plan", "apply", "plan.toml"]);
    assert!(err.contains("Did you mean \"schema\""), "{err}");

    // A ring, which nothing could ever start.
    sandbox.write_file(
        "plan.toml",
        r#"
plan = "p"
[[task]]
name = "a"
title = "A"
needs = ["b"]
[[task]]
name = "b"
title = "B"
needs = ["a"]
"#,
    );
    let err = sandbox.run_failing(&["plan", "apply", "plan.toml"]);
    assert!(err.contains("in a circle"), "{err}");

    // Not valid TOML at all: the error points at the line.
    sandbox.write_file("plan.toml", "plan = \"p\"\n[[task]]\nnaem = \"a\"\n");
    let err = sandbox.run_failing(&["plan", "apply", "plan.toml"]);
    assert!(err.contains("unknown field `naem`"), "{err}");

    assert_eq!(sandbox.run(&["ls"]).lines().count(), 1, "nothing was filed");
}

#[test]
fn a_plan_can_be_read_from_stdin() {
    use std::io::Write as _;
    use std::process::Stdio;

    let sandbox = Sandbox::new();
    let mut child = sandbox
        .command()
        .args(["plan", "apply", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hird");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PLAN.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("filed 5 tasks"));
}

#[test]
fn agents_work_a_filed_plan_the_way_they_work_anything_else() {
    let sandbox = Sandbox::new();
    sandbox.write_file("plan.toml", PLAN);
    sandbox.run(&["plan", "apply", "plan.toml"]);

    // "work the queue", twice over. The schema wins on priority; the second
    // agent cannot have the port, which waits for it.
    let mut codex = McpSession::start(&sandbox, "codex");
    let first = codex.call("task_next", serde_json::json!({})).unwrap();
    assert_eq!(first["claimed"]["claimed"].as_i64(), Some(1), "{first}");

    let mut claude = McpSession::start(&sandbox, "claude-code");
    let second = claude.call("task_next", serde_json::json!({})).unwrap();
    assert_eq!(
        second["claimed"]["claimed"].as_i64(),
        Some(3),
        "the port is blocked, so this is the renderer: {second}"
    );

    // And the pair the preview warned about is exactly the one now deferred:
    // the audit overlaps the renderer another agent is holding.
    let mut copilot = McpSession::start(&sandbox, "copilot");
    let third = copilot.call("task_next", serde_json::json!({})).unwrap();
    assert!(third["claimed"].is_null(), "{third}");
    assert_eq!(third["deferred"][0]["seq"].as_i64(), Some(4), "{third}");

    codex.shutdown();
    claude.shutdown();
    copilot.shutdown();
}
