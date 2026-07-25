//! Drives `hird mcp` as a real subprocess, speaking JSON-RPC over its stdio.
//!
//! This is the contract a harness actually depends on, so the test does no
//! in-process shortcuts: it spawns the binary, initializes, lists tools and
//! calls them, exactly as Claude Code or Codex would.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};
use support::Sandbox;

/// A live `hird mcp` subprocess.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(sandbox: &Sandbox, harness: &str) -> McpSession {
        let mut command: Command = sandbox.command();
        let mut child = command
            .arg("mcp")
            .env("HIRD_HARNESS", harness)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hird mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = McpSession {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        session.initialize();
        session
    }

    fn send(&mut self, message: &Value) {
        let line = serde_json::to_string(message).unwrap();
        writeln!(self.stdin, "{line}").expect("write to hird mcp");
        self.stdin.flush().expect("flush");
    }

    /// Read lines until one carries the id we are waiting for.
    fn read_response(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("read from hird mcp");
            assert!(read > 0, "hird mcp closed stdout while awaiting id {id}");
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("not JSON-RPC: {line:?} ({e})"));
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
            // Notifications and unrelated traffic are ignored.
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.read_response(id)
    }

    fn initialize(&mut self) -> Value {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "0"},
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    /// Call a tool and return the parsed JSON of its text content.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        if let Some(error) = response.get("error") {
            panic!("{name} returned a protocol error: {error}");
        }
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {result}"))
            .to_string();
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(text);
        }
        Ok(serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{name} result is not JSON: {text:?} ({e})")))
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

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
fn tools_list_returns_exactly_the_eight_designed_tools() {
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
#[test]
fn mcp_mode_starts_well_inside_the_startup_budget() {
    const BUDGET_MS: u128 = 50;
    const RUNS: usize = 9;

    let sandbox = Sandbox::new();
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
    let mut command = sandbox.command();
    let mut child = command
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut session = McpSession {
        child,
        stdin,
        stdout,
        next_id: 0,
    };
    session.initialize();

    let claimed = session.call("task_claim", json!({"seq": seq})).unwrap();
    assert!(
        claimed["holder"].as_str().unwrap().starts_with("unknown:"),
        "{}",
        claimed["holder"]
    );
    session.shutdown();
}
