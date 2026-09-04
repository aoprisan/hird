//! `hird web` end to end: the real binary, a real socket, the page and its
//! API — including the event stream a page follows.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use support::Sandbox;

/// A running `hird web`, killed when the test ends.
struct Viewer {
    child: Child,
    addr: String,
    /// Held open for the viewer's whole life: the banner is written line by
    /// line, and a pipe closed after the first line would end the server
    /// with a broken pipe before it ever accepted a connection.
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Viewer {
    fn start(sandbox: &Sandbox) -> Viewer {
        let mut cmd: Command = sandbox.command();
        cmd.args(["web", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn hird web");
        let stdout = child.stdout.take().expect("stdout");
        let mut stdout = BufReader::new(stdout);
        let mut banner = String::new();
        stdout.read_line(&mut banner).expect("a banner line");
        let url = banner
            .trim_end()
            .strip_prefix("hird web: ")
            .unwrap_or_else(|| panic!("unexpected banner {banner:?}"));
        let addr = url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        // The rest of the banner, so the server is past its greeting.
        for _ in 0..3 {
            let mut line = String::new();
            stdout.read_line(&mut line).expect("banner");
        }
        Viewer {
            child,
            addr,
            _stdout: stdout,
        }
    }

    /// One request, the whole response: status line, headers, body.
    fn get(&self, path: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("a header block");
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status");
        (status, head.to_string(), body.to_string())
    }

    fn json(&self, path: &str) -> Value {
        let (status, head, body) = self.get(path);
        assert_eq!(status, 200, "{head}\n{body}");
        assert!(head.contains("application/json"), "{head}");
        serde_json::from_str(&body).expect("json body")
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_page_and_the_graph_are_served_read_only() {
    let sb = Sandbox::new();
    sb.run(&["add", "Design the schema", "--path", "src/db.rs"]);
    sb.run(&[
        "add",
        "Port the repos",
        "--needs",
        "1",
        "--requires",
        "browser",
    ]);
    let viewer = Viewer::start(&sb);

    let (status, head, body) = viewer.get("/");
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("text/html"), "{head}");
    assert!(
        body.contains("<title>hird</title>"),
        "not the page:\n{body}"
    );

    let graph = viewer.json("/api/graph");
    assert_eq!(graph["live"], true);
    assert_eq!(graph["waves"], serde_json::json!([[1], [2]]));
    assert_eq!(graph["edges"][0]["from"], 1);
    assert_eq!(graph["edges"][0]["to"], 2);
    let repos = graph["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["seq"] == 2)
        .unwrap();
    assert_eq!(repos["waits_for"], serde_json::json!([1]));
    assert_eq!(repos["requires"], serde_json::json!(["browser"]));

    let task = viewer.json("/api/task/1");
    assert!(task["show"].as_str().unwrap().contains("Design the schema"));
    assert!(task["why"].as_str().unwrap().contains("claimable"));

    let (status, _, _) = viewer.get("/api/task/99");
    assert_eq!(status, 404);
    let (status, _, _) = viewer.get("/nope");
    assert_eq!(status, 404);

    // Nothing writes. A POST is turned away before it is even read.
    let mut stream = TcpStream::connect(&viewer.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "POST /api/task/1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 405"), "{raw}");
}

#[test]
fn the_graph_can_be_replayed_and_narrowed_to_a_plan() {
    let sb = Sandbox::new();
    let plan = sb.dir.path().join("plan.toml");
    std::fs::write(
        &plan,
        r#"
plan = "p"
[[task]]
name = "a"
title = "First"
[[task]]
name = "b"
title = "Second"
needs = ["a"]
"#,
    )
    .unwrap();
    sb.run(&["plan", "apply", plan.to_str().unwrap()]);
    sb.run(&["add", "Loose end"]);
    let viewer = Viewer::start(&sb);

    let narrowed = viewer.json("/api/graph?plan=p");
    let seqs: Vec<i64> = narrowed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs, vec![1, 2]);
    assert_eq!(narrowed["plans"], serde_json::json!(["p"]));

    let before = viewer.json("/api/graph?at=2000-01-01");
    assert_eq!(before["live"], false);
    assert_eq!(before["tasks"].as_array().unwrap().len(), 0);

    let (status, _, body) = viewer.get("/api/graph?at=yesterday");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("error"), "{body}");
}

#[test]
fn the_feed_streams_what_happens_after_the_page_opened() {
    let sb = Sandbox::new();
    sb.run(&["add", "Already there"]);
    let viewer = Viewer::start(&sb);

    let mut stream = TcpStream::connect(&viewer.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(stream, "GET /api/feed HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    // Past the headers and the greeting.
    let mut hello = None;
    for _ in 0..20 {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.starts_with("data:") {
            hello = Some(line.clone());
            break;
        }
    }
    let hello = hello.expect("a hello event");
    assert!(hello.contains("\"cursor\""), "{hello}");

    // Something happens on the board after the stream opened.
    sb.run(&["add", "Filed while watching"]);
    let mut event = None;
    for _ in 0..40 {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.starts_with("data:") {
            event = Some(line.clone());
            break;
        }
    }
    let event = event.expect("the filing to reach the stream");
    let json: Value = serde_json::from_str(event.trim_start_matches("data:").trim()).unwrap();
    assert_eq!(json["kind"], "created");
    assert_eq!(json["title"], "Filed while watching");
    assert_eq!(json["task"], 2);
}
