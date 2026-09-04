//! `hird web`: the board in a browser — a viewer, not a transport.
//!
//! The TUI is a screen you sit at, and a terminal is a poor medium for a
//! dependency graph past ten nodes: edges cross, and cards run out of room
//! to say what they feed. `hird web` draws the same board as a picture —
//! nodes by wave, arrows for dependencies, holders on the nodes, the feed
//! streaming underneath and a scrubber over the trail — and it does so under
//! exactly the TUI's posture:
//!
//! - **It binds `127.0.0.1` and dies with the terminal.** One foreground
//!   process per session, like `hird tui`; nothing stays up, nothing is
//!   installed, nothing has accounts.
//! - **It is read-only.** Every route is a `GET`, and the page has no button
//!   that writes. Cancelling, reopening and answering stay with the CLI and
//!   the TUI, where the human already is.
//! - **No agent ever talks to it.** Harnesses reach the queue over MCP on
//!   stdio, exactly as before; the page reads the same SQLite file the TUI
//!   does and the queue does not know it is being watched.
//!
//! That is the line between this and the HTTP transport the roadmap defers:
//! a transport is something an agent depends on to reach the queue, and this
//! is something a human looks at. It polls twice a second, sweeps expired
//! leases as it goes (announcing them, when a dispatch hook is configured,
//! so a board left open in a browser never swallows a summons), and streams
//! the trail to the page as server-sent events.
//!
//! Hand-rolled on `std::net` on purpose: four routes and one event stream do
//! not justify an HTTP framework in the dependency tree of a binary whose
//! startup time harnesses measure.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::Db;
use crate::graph::{event_json, Snapshot};
use crate::herald::{sweep_announcing, Herald};
use crate::identity::{self, ACTOR_WEB};
use crate::repo::{FeedFilter, ProjectScope};
use crate::witness;

/// The page, embedded so the binary stays one file.
const INDEX: &str = include_str!("web/index.html");

/// How often the page is told what changed, and leases are swept.
const POLL: Duration = Duration::from_millis(500);
/// How often the working tree is looked at, as in the TUI.
const LOOK: Duration = Duration::from_millis(2_000);
/// How often a quiet event stream says it is still there.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// What `hird web` was asked for.
#[derive(Debug, Clone)]
pub struct Options {
    /// The address to listen on. Loopback unless somebody says otherwise.
    pub bind: String,
    /// The port; `0` lets the system pick one, which the banner reports.
    pub port: u16,
    /// Show every project rather than the one this directory resolves to.
    pub all_projects: bool,
}

struct State {
    db: Mutex<Db>,
    config: Config,
    project: String,
    all_projects: bool,
    herald: Option<Herald>,
}

/// A bound listener, not yet serving. Split from [`Server::run`] so the
/// address is known — and printable — before the first request.
pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
    db_path: PathBuf,
}

impl Server {
    /// Bind and open the database; nothing is served until [`Server::run`].
    pub fn bind(db_path: &Path, config: Config, options: &Options) -> anyhow::Result<Server> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project = identity::resolve_project(&cwd);
        let db = Db::open(db_path)?;
        let herald = config.herald(db_path);
        let listener = TcpListener::bind((options.bind.as_str(), options.port)).map_err(|e| {
            anyhow::anyhow!("cannot listen on {}:{}: {e}", options.bind, options.port)
        })?;
        let all_projects = config.all_projects(Some(options.all_projects));
        Ok(Server {
            listener,
            state: Arc::new(State {
                db: Mutex::new(db),
                config,
                project,
                all_projects,
                herald,
            }),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Where the page is.
    pub fn url(&self) -> String {
        match self.listener.local_addr() {
            Ok(addr) => format!("http://{addr}/"),
            Err(_) => "http://?/".to_string(),
        }
    }

    /// Say where the page is, then serve until the process is stopped.
    pub fn run(self, out: &mut impl Write) -> anyhow::Result<()> {
        writeln!(out, "hird web: {}", self.url())?;
        writeln!(
            out,
            "  board     {}",
            if self.state.all_projects {
                "every project".to_string()
            } else {
                self.state.project.clone()
            }
        )?;
        writeln!(out, "  database  {}", self.db_path.display())?;
        writeln!(out, "  read-only; Ctrl-C stops it")?;
        out.flush()?;

        let poller = Arc::clone(&self.state);
        std::thread::spawn(move || poll(&poller));

        for stream in self.listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                // A client that hung up mid-response is not an error worth
                // anything but ending its thread.
                let _ = handle(stream, &state);
            });
        }
        Ok(())
    }
}

/// What the TUI does twice a second, done here for as long as the page is
/// served: sweep expired leases so the feed shows them, announce them when a
/// hook is wired, and look at the working tree now and then.
fn poll(state: &State) {
    let witness = state.config.witness(Path::new(&state.project));
    let mut last_look = Instant::now();
    loop {
        std::thread::sleep(POLL);
        let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
        if state.herald.is_some() {
            sweep_announcing(&db, &state.config, state.herald.as_ref());
        } else {
            let _ = db.tasks().sweep_leases();
        }
        if let Some(witness) = &witness {
            if last_look.elapsed() >= LOOK {
                last_look = Instant::now();
                let _ = witness::sweep(&db, witness, &state.project, ACTOR_WEB);
            }
        }
    }
}

// ------------------------------------------------------------------ requests

fn handle(mut stream: TcpStream, state: &State) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    // The headers are read past and ignored: nothing here negotiates.
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        return respond(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            b"hird web is read-only\n",
        );
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params = parse_query(query);
    let param = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    let scope = ProjectScope::resolve(
        &state.project,
        param("all").is_some_and(|v| v == "1" || v == "true") || state.all_projects,
    );

    match path {
        "/" | "/index.html" => respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            INDEX.as_bytes(),
        ),
        "/api/graph" => {
            let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
            let plan = param("plan").filter(|p| !p.is_empty());
            let snapshot = match param("at").filter(|at| !at.is_empty()) {
                Some(when) => match crate::cli::parse_when(when, chrono::Utc::now()) {
                    Ok(cutoff) => Snapshot::at(&db, &scope, &cutoff, plan),
                    Err(e) => return json_error(&mut stream, 400, &e.to_string()),
                },
                None => Snapshot::live(&db, &scope, plan),
            };
            match snapshot {
                Ok(snapshot) => json(&mut stream, 200, &serde_json::to_value(&snapshot)?),
                Err(e) => json_error(&mut stream, 500, &e.to_string()),
            }
        }
        "/api/events" => {
            let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
            let limit = param("limit")
                .and_then(|l| l.parse::<usize>().ok())
                .unwrap_or(60)
                .clamp(1, 1_000);
            match db.events().tail(&scope, &FeedFilter::default(), limit) {
                Ok(events) => {
                    let list: Vec<_> = events.iter().map(event_json).collect();
                    json(&mut stream, 200, &serde_json::Value::Array(list))
                }
                Err(e) => json_error(&mut stream, 500, &e.to_string()),
            }
        }
        "/api/feed" => {
            let after = param("after").and_then(|c| c.parse::<i64>().ok());
            feed(stream, state, &scope, after)
        }
        _ => match path
            .strip_prefix("/api/task/")
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(seq) => {
                let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
                let mut show = Vec::new();
                let mut why = Vec::new();
                if let Err(e) = crate::cli::show(&db, seq, &state.config, &mut show) {
                    return json_error(&mut stream, 404, &format!("{e:#}"));
                }
                if let Err(e) = crate::cli::why(&db, seq, &state.config, &mut why) {
                    return json_error(&mut stream, 404, &format!("{e:#}"));
                }
                json(
                    &mut stream,
                    200,
                    &serde_json::json!({
                        "seq": seq,
                        "show": String::from_utf8_lossy(&show),
                        "why": String::from_utf8_lossy(&why),
                    }),
                )
            }
            None => respond(
                &mut stream,
                404,
                "text/plain; charset=utf-8",
                b"not found\n",
            ),
        },
    }
}

/// The trail as server-sent events: everything after `after` (or after now),
/// then whatever lands, at the board's cadence, until the page goes away.
fn feed(
    mut stream: TcpStream,
    state: &State,
    scope: &ProjectScope,
    after: Option<i64>,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
         Connection: close\r\n\r\n"
    )?;
    let filter = FeedFilter::default();
    let mut cursor = match after {
        Some(cursor) => cursor,
        None => {
            let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
            db.events()
                .bounds(scope)
                .ok()
                .flatten()
                .map(|(_, cursor)| cursor)
                .unwrap_or(0)
        }
    };
    write!(
        stream,
        "retry: 1000\nevent: hello\ndata: {{\"cursor\":{cursor}}}\n\n"
    )?;
    stream.flush()?;
    let mut last_word = Instant::now();
    loop {
        std::thread::sleep(POLL);
        let fresh = {
            let db = state.db.lock().unwrap_or_else(PoisonError::into_inner);
            db.events().since(scope, &filter, cursor)
        };
        let Ok(fresh) = fresh else {
            continue;
        };
        for event in &fresh {
            cursor = event.cursor;
            write!(stream, "data: {}\n\n", event_json(event))?;
            last_word = Instant::now();
        }
        if last_word.elapsed() >= KEEPALIVE {
            write!(stream, ": still here\n\n")?;
            last_word = Instant::now();
        }
        // A write to a closed socket is what ends this loop; flushing is how
        // it finds out.
        stream.flush()?;
    }
}

// ----------------------------------------------------------------- responses

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn json(stream: &mut TcpStream, status: u16, value: &serde_json::Value) -> std::io::Result<()> {
    respond(
        stream,
        status,
        "application/json",
        value.to_string().as_bytes(),
    )
}

fn json_error(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    json(stream, status, &serde_json::json!({ "error": message }))
}

/// `a=1&b=two%20words` as pairs, percent-decoded, in order.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&text[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_are_split_and_decoded() {
        assert_eq!(
            parse_query("plan=serde%2Dmigration&at=2026-08-14T10%3A00&all=1"),
            vec![
                ("plan".to_string(), "serde-migration".to_string()),
                ("at".to_string(), "2026-08-14T10:00".to_string()),
                ("all".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(parse_query(""), Vec::<(String, String)>::new());
        assert_eq!(
            parse_query("flag"),
            vec![("flag".to_string(), String::new())]
        );
    }

    #[test]
    fn percent_decoding_tolerates_bad_escapes() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%2"), "%2");
    }
}
