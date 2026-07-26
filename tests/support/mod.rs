//! Shared helpers for the integration tests, which drive the real binary.
//!
//! Each test binary includes this module, so not every helper is used by
//! every one of them.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

/// A scratch home + project + database for one test.
pub struct Sandbox {
    pub dir: TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        Sandbox { dir }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.path().join("hird.db")
    }

    pub fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// A command with the environment pinned so tests never touch the real
    /// user's config or data directories.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.current_dir(self.project())
            .env_remove("HIRD_HARNESS")
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("HIRD_DB", self.db())
            .env("HIRD_PROJECT", self.project());
        cmd
    }

    /// Run a `hird` subcommand and require success.
    pub fn run(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("spawn hird");
        assert!(
            output.status.success(),
            "hird {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }

    /// Run a `hird` subcommand and require failure, returning stderr.
    pub fn run_failing(&self, args: &[&str]) -> String {
        let output: Output = self.command().args(args).output().expect("spawn hird");
        assert!(
            !output.status.success(),
            "hird {args:?} unexpectedly succeeded"
        );
        String::from_utf8(output.stderr).expect("utf-8 stderr")
    }

    /// Write a config file into the sandbox's XDG config home.
    pub fn write_config(&self, contents: &str) {
        let dir = self.dir.path().join("config").join("hird");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }

    /// Turn the sandbox project into a git repository with one commit, so the
    /// witness has something to watch. Without this a sandbox is a plain
    /// directory and witnessing stays off, which is also a case worth testing.
    pub fn git_init(&self) {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "t"][..],
        ] {
            self.git(args);
        }
        self.write_file("README.md", "# sandbox\n");
        self.write_file("src/config.rs", "// config\n");
        self.commit("initial");
    }

    pub fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.project())
            .output()
            .expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Write a file inside the project, creating parent directories.
    pub fn write_file(&self, rel: &str, contents: &str) {
        let path = self.project().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

/// Path to the binary under test, as produced by cargo for this test run.
pub fn bin() -> PathBuf {
    // `current_exe` is target/<profile>/deps/<test>-<hash>; the binary sits two
    // levels up.
    let mut path = std::env::current_exe().expect("current exe");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("hird{}", std::env::consts::EXE_SUFFIX))
}

/// Assert a path exists, with a message naming it.
pub fn assert_exists(path: &Path) {
    assert!(path.exists(), "{} does not exist", path.display());
}

/// A live `hird mcp` subprocess.
pub struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    pub fn start(sandbox: &Sandbox, harness: &str) -> McpSession {
        McpSession::spawn(sandbox, Some(harness))
    }

    /// A session started the way a harness that forgot to set `HIRD_HARNESS`
    /// starts one.
    pub fn start_unnamed(sandbox: &Sandbox) -> McpSession {
        McpSession::spawn(sandbox, None)
    }

    fn spawn(sandbox: &Sandbox, harness: Option<&str>) -> McpSession {
        let mut command: Command = sandbox.command();
        if let Some(harness) = harness {
            command.env("HIRD_HARNESS", harness);
        }
        let mut child = command
            .arg("mcp")
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

    pub fn send(&mut self, message: &Value) {
        let line = serde_json::to_string(message).unwrap();
        writeln!(self.stdin, "{line}").expect("write to hird mcp");
        self.stdin.flush().expect("flush");
    }

    /// Read lines until one carries the id we are waiting for.
    pub fn read_response(&mut self, id: i64) -> Value {
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

    pub fn request(&mut self, method: &str, params: Value) -> Value {
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

    pub fn initialize(&mut self) -> Value {
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
    pub fn call(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
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

    /// Claim a task, with whatever it was filed as expecting to touch.
    ///
    /// The shortcut most tests want: they need a live lease held by a real
    /// session, not a particular claim payload.
    pub fn claim(&mut self, seq: i64) -> Value {
        self.call("task_claim", json!({"seq": seq})).expect("claim")
    }

    pub fn shutdown(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}
