//! The MCP server: `hird mcp`.
//!
//! One process per harness session, speaking JSON-RPC over stdio. Every tool
//! result is compact JSON; every error is a sentence the model can relay to the
//! human verbatim.

use std::path::Path;
use std::sync::Mutex;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::Db;
use crate::error::Error;
use crate::identity::{self, AgentId};
use crate::model::{Assertion, Status, Task, TaskEvent, TaskSummary};
use crate::repo::{MemoryQuery, NewAssertion, ProjectScope};

/// Number of trailing events `task_get` returns.
const EVENT_WINDOW: usize = 20;

/// Serve the MCP protocol on stdio until the client disconnects.
pub async fn serve(db_path: &Path, config: Config) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let server = HirdMcp::new(
        Db::open(db_path)?,
        AgentId::from_env(),
        identity::resolve_project(&cwd),
        config,
    );
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// The MCP server state for one harness session.
pub struct HirdMcp {
    // `rusqlite::Connection` is `Send` but not `Sync`; the mutex is only ever
    // held inside synchronous closures, never across an await.
    db: Mutex<Db>,
    agent: AgentId,
    project: String,
    config: Config,
}

impl HirdMcp {
    pub fn new(db: Db, agent: AgentId, project: String, config: Config) -> HirdMcp {
        HirdMcp {
            db: Mutex::new(db),
            agent,
            project,
            config,
        }
    }

    /// The identity recorded on this session's claims and assertions.
    pub fn actor(&self) -> String {
        self.agent.as_actor()
    }

    /// The project scope this session defaults to.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Run a closure against the database.
    ///
    /// Deliberately a synchronous function: the guard is created and dropped
    /// entirely inside it, so it never lands in an async state machine.
    fn with_db<T>(&self, f: impl FnOnce(&Db) -> T) -> T {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        f(&db)
    }

    fn scope(&self, all_projects: Option<bool>) -> ProjectScope {
        ProjectScope::resolve(&self.project, self.config.all_projects(all_projects))
    }

    /// How often an agent should call `task_update` to keep its lease.
    fn heartbeat_minutes(&self) -> u64 {
        (self.config.lease_ttl_minutes / 2).max(1)
    }

    /// What a harness sees in `initialize`.
    ///
    /// This is the only chance to tell the model the queue's rules, so it
    /// states them plainly: tasks are named by number, claim before working,
    /// heartbeat before the lease runs out, and write down what you learn.
    fn instructions(&self) -> String {
        format!(
            "hird is a shared work queue and memory, used at the same time by other agents \
             in other harnesses and by a human watching a TUI.\n\
             \n\
             Tasks are referred to by their number (`seq`), the way the human says them: \
             \"pick up task 42\" means seq 42. Always quote that number back.\n\
             \n\
             Working a task:\n\
             1. `task_get` to read the full instructions.\n\
             2. `task_claim` BEFORE doing any work. The claim is atomic — if another agent \
             holds the task the call fails and tells you who has it; relay that to the human \
             instead of working the task anyway.\n\
             3. `task_update` with a short note as you go. A claim is a lease of {ttl} minutes \
             and every update renews it, so call it at least every {heartbeat} minutes or the \
             task returns to the pool and another agent may take it.\n\
             4. `task_complete` with a result summary, or `task_fail` with a reason. \
             Only the lease holder may do either.\n\
             \n\
             Memory: `mem_store` durable facts you learn — where something lives, why a \
             decision was made, what a command is — one assertion per call, in plain prose \
             that will still make sense to a different agent next week. Pass `task_seq` when \
             you learned it working a task. `mem_search` before exploring from scratch; \
             another agent may already have found the answer.\n\
             \n\
             Everything is scoped to the current project ({project}) unless you pass \
             `all_projects: true`.",
            ttl = self.config.lease_ttl_minutes,
            heartbeat = self.heartbeat_minutes(),
            project = self.project,
        )
    }
}

// ------------------------------------------------------------------ arguments

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskListArgs {
    /// Only tasks in this status: open, claimed, in_progress, done, failed or cancelled.
    #[serde(default)]
    pub status: Option<String>,
    /// Include tasks from every project rather than just the current one.
    #[serde(default)]
    pub all_projects: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeqArgs {
    /// The task number, as the human refers to it.
    pub seq: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateArgs {
    /// The task number.
    pub seq: i64,
    /// What you have done or found. Recorded in the task's history.
    pub note: String,
    /// Pass "in_progress" the first time you actually start work.
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCompleteArgs {
    /// The task number.
    pub seq: i64,
    /// A summary of what was done, for the human reading the board.
    pub result: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskFailArgs {
    /// The task number.
    pub seq: i64,
    /// Why the task could not be completed.
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemStoreArgs {
    /// One factual assertion, in plain prose.
    pub content: String,
    /// Optional comma-separated tags, e.g. "build,ci".
    #[serde(default)]
    pub tags: Option<String>,
    /// The task you learned this while working, if any.
    #[serde(default)]
    pub task_seq: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemSearchArgs {
    /// Search text. Full-text syntax is supported; plain words are fine.
    pub query: String,
    /// Maximum results to return. Defaults to 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Search every project rather than just the current one.
    #[serde(default)]
    pub all_projects: Option<bool>,
    /// Include assertions that have since been replaced.
    #[serde(default)]
    pub include_superseded: Option<bool>,
}

// -------------------------------------------------------------------- results

#[derive(Debug, Serialize)]
struct TaskListResult<'a> {
    project: &'a str,
    all_projects: bool,
    count: usize,
    tasks: Vec<TaskRow>,
    /// Tasks whose leases expired during this call and are open again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    released: Vec<i64>,
}

#[derive(Debug, Serialize)]
struct TaskRow {
    seq: i64,
    title: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<String>,
    priority: i64,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

impl TaskRow {
    fn from_summary(summary: TaskSummary, show_project: bool) -> TaskRow {
        TaskRow {
            seq: summary.seq,
            title: summary.title,
            status: summary.status,
            holder: summary.claimed_by,
            priority: summary.priority,
            updated_at: summary.updated_at,
            project: show_project.then_some(summary.project),
        }
    }
}

#[derive(Debug, Serialize)]
struct TaskDetail {
    seq: i64,
    project: String,
    title: String,
    body: String,
    status: Status,
    priority: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<EventRow>,
}

impl TaskDetail {
    fn new(task: Task, events: Vec<TaskEvent>) -> TaskDetail {
        TaskDetail {
            seq: task.seq,
            project: task.project,
            title: task.title,
            body: task.body,
            status: task.status,
            priority: task.priority,
            holder: task.claimed_by,
            lease_expires_at: task.lease_expires_at,
            result: task.result,
            created_at: task.created_at,
            updated_at: task.updated_at,
            events: events.into_iter().map(EventRow::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct EventRow {
    at: String,
    actor: String,
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    detail: String,
}

impl From<TaskEvent> for EventRow {
    fn from(e: TaskEvent) -> EventRow {
        EventRow {
            at: e.at,
            actor: e.actor,
            kind: e.kind.as_str().to_string(),
            detail: e.detail,
        }
    }
}

#[derive(Debug, Serialize)]
struct ClaimResult {
    claimed: i64,
    holder: String,
    lease_expires_at: String,
    title: String,
    body: String,
    priority: i64,
    project: String,
    /// Restated so the model does not have to remember the initialize text.
    reminder: String,
}

#[derive(Debug, Serialize)]
struct UpdateResult {
    seq: i64,
    status: Status,
    lease_expires_at: String,
    note_recorded: String,
}

#[derive(Debug, Serialize)]
struct FinishResult {
    seq: i64,
    status: Status,
    result: String,
}

#[derive(Debug, Serialize)]
struct MemStoreResult {
    id: String,
    project: String,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_seq: Option<i64>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct MemSearchResult<'a> {
    query: &'a str,
    project: &'a str,
    all_projects: bool,
    count: usize,
    assertions: Vec<AssertionRow>,
}

#[derive(Debug, Serialize)]
struct AssertionRow {
    id: String,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    actor: String,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    superseded: bool,
}

impl AssertionRow {
    fn new(a: Assertion, show_project: bool) -> AssertionRow {
        let tags = a.tag_list().into_iter().map(str::to_string).collect();
        AssertionRow {
            id: a.id,
            tags,
            content: a.content,
            actor: a.actor,
            created_at: a.created_at,
            project: show_project.then_some(a.project),
            superseded: a.superseded_by.is_some(),
        }
    }
}

// ---------------------------------------------------------------------- tools

#[tool_router]
impl HirdMcp {
    /// List tasks in the queue. Run this first to see what work exists.
    #[tool(name = "task_list")]
    async fn task_list(
        &self,
        Parameters(args): Parameters<TaskListArgs>,
    ) -> Result<String, String> {
        let status = match args
            .status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(raw.parse::<Status>().map_err(|e| e.to_string())?),
            None => None,
        };
        let scope = self.scope(args.all_projects);
        let show_project = scope.is_all();

        let (released, tasks) = self.with_db(|db| {
            let released = db
                .tasks()
                .sweep_leases()
                .map(|s| s.expired)
                .unwrap_or_default();
            (released, db.tasks().list(&scope, status))
        });
        let tasks = tasks.map_err(stringify)?;

        json(&TaskListResult {
            project: &self.project,
            all_projects: scope.is_all(),
            count: tasks.len(),
            tasks: tasks
                .into_iter()
                .map(|t| TaskRow::from_summary(t, show_project))
                .collect(),
            released,
        })
    }

    /// Read one task in full, including its instructions and recent history.
    #[tool(name = "task_get")]
    async fn task_get(&self, Parameters(args): Parameters<SeqArgs>) -> Result<String, String> {
        let detail = self.with_db(|db| {
            let task = db.tasks().get(args.seq)?;
            let events = db.tasks().events(&task.id, EVENT_WINDOW)?;
            Ok::<_, Error>(TaskDetail::new(task, events))
        });
        json(&detail.map_err(stringify)?)
    }

    /// Claim a task before working on it. Fails if someone else already holds it.
    #[tool(name = "task_claim")]
    async fn task_claim(&self, Parameters(args): Parameters<SeqArgs>) -> Result<String, String> {
        let actor = self.actor();
        let ttl = self.config.lease_ttl();
        let task = self
            .with_db(|db| db.tasks().claim(args.seq, &actor, ttl))
            .map_err(stringify)?;

        let heartbeat = self.heartbeat_minutes();
        json(&ClaimResult {
            claimed: task.seq,
            holder: actor,
            lease_expires_at: task.lease_expires_at.clone().unwrap_or_default(),
            title: task.title,
            body: task.body,
            priority: task.priority,
            project: task.project,
            reminder: format!(
                "call task_update at least every {heartbeat} minutes to keep this lease, \
                 then task_complete or task_fail when you are done"
            ),
        })
    }

    /// Record progress and renew your lease. Only the holder may call this.
    #[tool(name = "task_update")]
    async fn task_update(
        &self,
        Parameters(args): Parameters<TaskUpdateArgs>,
    ) -> Result<String, String> {
        let start = match args
            .status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => false,
            Some(raw) => match raw.parse::<Status>() {
                Ok(Status::InProgress) => true,
                _ => {
                    return Err(format!(
                        "task_update can only set status to \"in_progress\" (got {raw:?}); \
                         use task_complete or task_fail to finish a task"
                    ))
                }
            },
        };
        let actor = self.actor();
        let ttl = self.config.lease_ttl();
        let task = self
            .with_db(|db| db.tasks().update(args.seq, &actor, start, &args.note, ttl))
            .map_err(stringify)?;

        json(&UpdateResult {
            seq: task.seq,
            status: task.status,
            lease_expires_at: task.lease_expires_at.unwrap_or_default(),
            note_recorded: args.note.trim().to_string(),
        })
    }

    /// Finish a task you hold, with a summary of what was done.
    #[tool(name = "task_complete")]
    async fn task_complete(
        &self,
        Parameters(args): Parameters<TaskCompleteArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let task = self
            .with_db(|db| db.tasks().complete(args.seq, &actor, &args.result))
            .map_err(stringify)?;
        json(&FinishResult {
            seq: task.seq,
            status: task.status,
            result: task.result.unwrap_or_default(),
        })
    }

    /// Give up on a task you hold, saying why. The human can reopen it later.
    #[tool(name = "task_fail")]
    async fn task_fail(
        &self,
        Parameters(args): Parameters<TaskFailArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let task = self
            .with_db(|db| db.tasks().fail(args.seq, &actor, &args.reason))
            .map_err(stringify)?;
        json(&FinishResult {
            seq: task.seq,
            status: task.status,
            result: task.result.unwrap_or_default(),
        })
    }

    /// Record one durable fact so other agents and sessions can find it later.
    #[tool(name = "mem_store")]
    async fn mem_store(
        &self,
        Parameters(args): Parameters<MemStoreArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let assertion = self
            .with_db(|db| {
                db.memory().store(NewAssertion {
                    project: &self.project,
                    content: &args.content,
                    tags: args.tags.as_deref().unwrap_or(""),
                    actor: &actor,
                    task_seq: args.task_seq,
                })
            })
            .map_err(stringify)?;

        let tags = assertion
            .tag_list()
            .into_iter()
            .map(str::to_string)
            .collect();
        json(&MemStoreResult {
            id: assertion.id,
            project: assertion.project,
            tags,
            content: assertion.content,
            actor: assertion.actor,
            task_seq: args.task_seq,
            created_at: assertion.created_at,
        })
    }

    /// Search recorded facts before working something out from scratch.
    #[tool(name = "mem_search")]
    async fn mem_search(
        &self,
        Parameters(args): Parameters<MemSearchArgs>,
    ) -> Result<String, String> {
        let scope = self.scope(args.all_projects);
        let show_project = scope.is_all();
        let query = MemoryQuery::new(&args.query, scope.clone())
            .limit(args.limit.unwrap_or(20).clamp(1, 200))
            .include_superseded(args.include_superseded.unwrap_or(false));

        let hits = self
            .with_db(|db| db.memory().search(&query))
            .map_err(stringify)?;

        json(&MemSearchResult {
            query: &args.query,
            project: &self.project,
            all_projects: scope.is_all(),
            count: hits.len(),
            assertions: hits
                .into_iter()
                .map(|a| AssertionRow::new(a, show_project))
                .collect(),
        })
    }
}

#[tool_handler]
impl ServerHandler for HirdMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("hird", env!("CARGO_PKG_VERSION"))
                    .with_title("hird work queue & memory"),
            )
            .with_instructions(self.instructions())
    }
}

/// Render a payload as compact JSON.
fn json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("failed to encode result: {e}"))
}

/// Repository errors are already written as sentences for the model.
fn stringify(err: Error) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const PROJECT: &str = "/tmp/project";

    fn server() -> HirdMcp {
        HirdMcp::new(
            Db::open_in_memory().unwrap(),
            AgentId::new("claude-code", "af31"),
            PROJECT.to_string(),
            Config::default(),
        )
    }

    fn parse(raw: Result<String, String>) -> Value {
        serde_json::from_str(&raw.expect("tool call succeeded")).unwrap()
    }

    fn seed(server: &HirdMcp, title: &str, body: &str) -> i64 {
        server
            .with_db(|db| db.tasks().create(PROJECT, title, body, 0, "cli"))
            .unwrap()
            .seq
    }

    #[tokio::test]
    async fn task_list_reports_scope_and_rows() {
        let s = server();
        seed(&s, "write the parser", "full instructions here");

        let out = parse(
            s.task_list(Parameters(TaskListArgs {
                status: None,
                all_projects: None,
            }))
            .await,
        );

        assert_eq!(out["project"], PROJECT);
        assert_eq!(out["all_projects"], false);
        assert_eq!(out["count"], 1);
        assert_eq!(out["tasks"][0]["seq"], 1);
        assert_eq!(out["tasks"][0]["status"], "open");
        // Absent fields are omitted rather than serialized as null.
        assert!(out["tasks"][0].get("holder").is_none());
        assert!(out["tasks"][0].get("project").is_none());
    }

    #[tokio::test]
    async fn task_list_rejects_an_unknown_status_with_the_valid_set() {
        let s = server();
        let err = s
            .task_list(Parameters(TaskListArgs {
                status: Some("blocked".into()),
                all_projects: None,
            }))
            .await
            .unwrap_err();
        assert!(err.contains("unknown status"), "{err}");
        assert!(err.contains("in_progress"), "{err}");
    }

    #[tokio::test]
    async fn task_list_can_widen_to_every_project() {
        let s = server();
        seed(&s, "here", "");
        s.with_db(|db| db.tasks().create("/elsewhere", "there", "", 0, "cli"))
            .unwrap();

        let scoped = parse(
            s.task_list(Parameters(TaskListArgs {
                status: None,
                all_projects: None,
            }))
            .await,
        );
        assert_eq!(scoped["count"], 1);

        let wide = parse(
            s.task_list(Parameters(TaskListArgs {
                status: None,
                all_projects: Some(true),
            }))
            .await,
        );
        assert_eq!(wide["count"], 2);
        // Cross-project listings say which project each task belongs to.
        assert!(wide["tasks"][0]["project"].is_string());
    }

    #[tokio::test]
    async fn task_get_returns_the_body_and_history() {
        let s = server();
        let seq = seed(&s, "write the parser", "start with the lexer");

        let out = parse(s.task_get(Parameters(SeqArgs { seq })).await);
        assert_eq!(out["body"], "start with the lexer");
        assert_eq!(out["events"][0]["kind"], "created");
        assert_eq!(out["events"][0]["actor"], "cli");
    }

    #[tokio::test]
    async fn task_get_on_a_missing_task_is_a_plain_sentence() {
        let s = server();
        assert_eq!(
            s.task_get(Parameters(SeqArgs { seq: 99 }))
                .await
                .unwrap_err(),
            "task 99 not found"
        );
    }

    #[tokio::test]
    async fn claiming_returns_the_body_and_the_heartbeat_reminder() {
        let s = server();
        let seq = seed(&s, "write the parser", "start with the lexer");

        let out = parse(s.task_claim(Parameters(SeqArgs { seq })).await);
        assert_eq!(out["claimed"], seq);
        assert_eq!(out["holder"], "claude-code:af31");
        assert_eq!(out["body"], "start with the lexer");
        assert!(out["lease_expires_at"].as_str().unwrap().ends_with('Z'));
        assert!(out["reminder"].as_str().unwrap().contains("task_update"));
    }

    #[tokio::test]
    async fn a_losing_claim_names_the_holder_and_the_deadline() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.with_db(|db| {
            db.tasks()
                .claim(seq, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        let err = s.task_claim(Parameters(SeqArgs { seq })).await.unwrap_err();
        assert!(
            err.starts_with("task 1 is claimed by codex:9f2c until "),
            "{err}"
        );
    }

    #[tokio::test]
    async fn update_starts_work_and_renews_the_lease() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.task_claim(Parameters(SeqArgs { seq })).await.unwrap();

        let out = parse(
            s.task_update(Parameters(TaskUpdateArgs {
                seq,
                note: "  read the lexer  ".into(),
                status: Some("in_progress".into()),
            }))
            .await,
        );
        assert_eq!(out["status"], "in_progress");
        assert_eq!(out["note_recorded"], "read the lexer");
        assert!(out["lease_expires_at"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn update_refuses_to_be_used_as_a_finishing_move() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.task_claim(Parameters(SeqArgs { seq })).await.unwrap();

        let err = s
            .task_update(Parameters(TaskUpdateArgs {
                seq,
                note: "n".into(),
                status: Some("done".into()),
            }))
            .await
            .unwrap_err();
        assert!(err.contains("task_complete or task_fail"), "{err}");
    }

    #[tokio::test]
    async fn only_the_holder_can_update_or_finish() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.with_db(|db| {
            db.tasks()
                .claim(seq, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        for err in [
            s.task_update(Parameters(TaskUpdateArgs {
                seq,
                note: "n".into(),
                status: None,
            }))
            .await
            .unwrap_err(),
            s.task_complete(Parameters(TaskCompleteArgs {
                seq,
                result: "r".into(),
            }))
            .await
            .unwrap_err(),
            s.task_fail(Parameters(TaskFailArgs {
                seq,
                reason: "r".into(),
            }))
            .await
            .unwrap_err(),
        ] {
            assert!(err.contains("held by codex:9f2c"), "{err}");
        }
    }

    #[tokio::test]
    async fn completing_and_failing_report_the_final_state() {
        let s = server();
        let done = seed(&s, "a", "");
        s.task_claim(Parameters(SeqArgs { seq: done }))
            .await
            .unwrap();
        let out = parse(
            s.task_complete(Parameters(TaskCompleteArgs {
                seq: done,
                result: "merged".into(),
            }))
            .await,
        );
        assert_eq!(out["status"], "done");
        assert_eq!(out["result"], "merged");

        let bad = seed(&s, "b", "");
        s.task_claim(Parameters(SeqArgs { seq: bad }))
            .await
            .unwrap();
        let out = parse(
            s.task_fail(Parameters(TaskFailArgs {
                seq: bad,
                reason: "no credentials".into(),
            }))
            .await,
        );
        assert_eq!(out["status"], "failed");
        assert_eq!(out["result"], "no credentials");
    }

    #[tokio::test]
    async fn mem_store_records_provenance_and_the_task_link() {
        let s = server();
        let seq = seed(&s, "t", "");

        let out = parse(
            s.mem_store(Parameters(MemStoreArgs {
                content: "the lexer lives in src/lex.rs".into(),
                tags: Some(" parser , code ".into()),
                task_seq: Some(seq),
            }))
            .await,
        );
        assert_eq!(out["actor"], "claude-code:af31");
        assert_eq!(out["project"], PROJECT);
        assert_eq!(out["tags"], serde_json::json!(["parser", "code"]));
        assert_eq!(out["task_seq"], seq);
    }

    #[tokio::test]
    async fn mem_store_rejects_a_link_to_a_task_that_does_not_exist() {
        let s = server();
        let err = s
            .mem_store(Parameters(MemStoreArgs {
                content: "x".into(),
                tags: None,
                task_seq: Some(404),
            }))
            .await
            .unwrap_err();
        assert_eq!(err, "task 404 not found");
    }

    #[tokio::test]
    async fn mem_search_finds_what_mem_store_wrote() {
        let s = server();
        s.mem_store(Parameters(MemStoreArgs {
            content: "the lexer lives in src/lex.rs".into(),
            tags: None,
            task_seq: None,
        }))
        .await
        .unwrap();

        let out = parse(
            s.mem_search(Parameters(MemSearchArgs {
                query: "lexer".into(),
                limit: None,
                all_projects: None,
                include_superseded: None,
            }))
            .await,
        );
        assert_eq!(out["count"], 1);
        assert_eq!(
            out["assertions"][0]["content"],
            "the lexer lives in src/lex.rs"
        );
        assert_eq!(out["assertions"][0]["actor"], "claude-code:af31");
    }

    #[tokio::test]
    async fn mem_search_clamps_absurd_limits() {
        let s = server();
        for i in 0..5 {
            s.mem_store(Parameters(MemStoreArgs {
                content: format!("fact {i} about widgets"),
                tags: None,
                task_seq: None,
            }))
            .await
            .unwrap();
        }
        let out = parse(
            s.mem_search(Parameters(MemSearchArgs {
                query: "widgets".into(),
                limit: Some(0),
                all_projects: None,
                include_superseded: None,
            }))
            .await,
        );
        assert_eq!(out["count"], 1, "limit 0 is clamped to 1, not to nothing");
    }

    #[tokio::test]
    async fn task_list_reports_leases_it_released() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.with_db(|db| {
            db.tasks()
                .claim(seq, "codex:dead", Config::default().lease_ttl())
                .unwrap();
            db.conn()
                .execute(
                    "UPDATE tasks SET lease_expires_at = '2000-01-01T00:00:00.000Z' WHERE seq = ?1",
                    [seq],
                )
                .unwrap();
        });

        let out = parse(
            s.task_list(Parameters(TaskListArgs {
                status: None,
                all_projects: None,
            }))
            .await,
        );
        assert_eq!(out["released"], serde_json::json!([seq]));
        assert_eq!(out["tasks"][0]["status"], "open");
    }

    #[test]
    fn the_instructions_state_the_queue_rules() {
        let text = server().instructions();
        for expected in [
            "seq",
            "task_claim",
            "BEFORE",
            "task_update",
            "task_complete",
            "mem_store",
            "mem_search",
            "all_projects",
            PROJECT,
        ] {
            assert!(text.contains(expected), "instructions omit {expected:?}");
        }
        // The heartbeat interval must be derived from the configured TTL.
        assert!(text.contains("15 minutes"), "{text}");
        assert!(text.contains("every 7 minutes"), "{text}");
    }

    #[test]
    fn server_info_advertises_tools_and_carries_the_instructions() {
        let s = server();
        let info = s.get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "hird");
        assert_eq!(
            info.instructions.as_deref(),
            Some(s.instructions().as_str())
        );
    }

    #[test]
    fn exactly_the_eight_designed_tools_are_exposed() {
        let mut names: Vec<String> = HirdMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
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
    }

    #[test]
    fn every_tool_has_a_description_and_an_input_schema() {
        for tool in HirdMcp::tool_router().list_all() {
            let name = &tool.name;
            assert!(
                tool.description.as_ref().is_some_and(|d| d.len() > 20),
                "{name} needs a real description"
            );
            assert!(
                tool.input_schema.contains_key("properties")
                    || tool.input_schema.contains_key("type"),
                "{name} has no input schema"
            );
        }
    }
}
