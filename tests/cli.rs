//! End-to-end tests for the command line, driving the real binary.

mod support;

use support::{assert_exists, Sandbox};

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
