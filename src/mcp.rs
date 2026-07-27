//! The MCP server: `hird mcp`.
//!
//! One process per harness session, speaking JSON-RPC over stdio. Every tool
//! result is compact JSON; every error is a sentence the model can relay to the
//! human verbatim.

use std::collections::BTreeMap;
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

use crate::footing;
use crate::identity::{self, AgentId};
use crate::model::{
    Assertion, Blocker, Conflict, Contention, Observed, Recusal, Standing, Status, Task, TaskEvent,
    TaskSummary,
};
use crate::repo::{Claim, Dispatch, MemoryQuery, NewAssertion, ProjectScope, Recalled, Subtask};
use crate::witness::{self, Witness};

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
    /// Resolved once at startup: whether this project is somewhere the working
    /// tree can be watched. `None` is an ordinary state, not a degraded one.
    witness: Option<Witness>,
}

impl HirdMcp {
    pub fn new(db: Db, agent: AgentId, project: String, config: Config) -> HirdMcp {
        let witness = config.witness(Path::new(&project));
        HirdMcp {
            db: Mutex::new(db),
            agent,
            project,
            config,
            witness,
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

    /// Look at the working tree on this task's behalf and report what it says.
    ///
    /// The order is the point. The sweep brings every live task's footprint up
    /// to date without confirming anything; the report is read off that; and
    /// only then is this task's holder recorded as having been shown those
    /// versions. Confirming first would mark the agent's copy current in the
    /// same breath as telling it the file had moved, which is to say it would
    /// never tell it anything.
    ///
    /// Best-effort in the strongest sense: a sweep that fails costs the agent a
    /// report it did not ask for, while propagating the failure would cost it
    /// the call it did make. Nothing here can turn a completed task into an
    /// error, and there is no configuration in which it can.
    fn witnessed(&self, db: &Db, seq: i64) -> Evidence {
        let Some(witness) = &self.witness else {
            return Evidence::default();
        };
        let swept = witness::sweep(db, witness, &self.project, &self.actor()).ok();
        let mut evidence = self.evidence(db, seq);
        if let Some(swept) = swept {
            let _ = db.witnessed().confirm(seq, swept.changes_for(seq));
            if swept.tree.truncated {
                evidence.partial = Some(
                    "the working tree has more uncommitted files than hird will fingerprint \
                     in one go, so `changed` is a floor and not the whole story"
                        .to_string(),
                );
            }
        }
        evidence
    }

    /// The witness memory may read the tree through, if the configuration
    /// lets it. Narrower than `self.witness` by one flag.
    fn footing(&self) -> Option<&Witness> {
        self.config.footing(self.witness.as_ref())
    }

    /// The memory relevant to a task, each fact carrying whether the code it
    /// was learned from still says what it said.
    ///
    /// Recall is a courtesy on top of the real answer, so it never turns a
    /// successful claim into an error: a failure here costs the agent some
    /// context, while propagating it would cost it the task it just took.
    fn recall(&self, db: &Db, seq: i64, limit: usize) -> Vec<Recalled> {
        let recalled = db.recall().for_task(seq, limit).unwrap_or_default();
        footing::decorate(db, self.footing(), recalled)
    }

    /// Start watching the tree on behalf of a task this session just claimed.
    fn begin_witnessing(&self, db: &Db, seq: i64) {
        let Some(witness) = &self.witness else {
            return;
        };
        let _ = witness::begin(db, witness, &self.project, seq, &self.actor());
    }

    /// What the witness has to say about a task, as the tools report it.
    fn evidence(&self, db: &Db, seq: i64) -> Evidence {
        if self.witness.is_none() {
            return Evidence::default();
        }
        Evidence {
            changed: db
                .witnessed()
                .touched(seq)
                .map(|seen| seen.iter().map(Observed::describe).collect())
                .unwrap_or_default(),
            contended: db
                .witnessed()
                .contention(seq)
                .map(|seen| seen.iter().map(Contention::describe).collect())
                .unwrap_or_default(),
            undeclared: db.witnessed().undeclared(seq).unwrap_or_default(),
            partial: None,
            alone: db
                .witnessed()
                .baselines(&self.project)
                .map(|live| live.len() <= 1)
                .unwrap_or(true),
        }
    }

    /// How often an agent should call `task_update` to keep its lease.
    fn heartbeat_minutes(&self) -> u64 {
        (self.config.lease_ttl_minutes / 2).max(1)
    }

    /// What a harness sees in `initialize`.
    ///
    /// This is the only chance to tell the model the queue's rules, so it
    /// states them plainly: how to be handed work, claim before working,
    /// heartbeat before the lease runs out, say which files you are in, and
    /// write down what you learn.
    fn instructions(&self) -> String {
        format!(
            "hird is a shared work queue and memory, used at the same time by other agents \
             in other harnesses and by a human watching a TUI. Assume you are not alone.\n\
             \n\
             Tasks are referred to by their number (`seq`), the way the human says them: \
             \"pick up task 42\" means seq 42. Always quote that number back.\n\
             \n\
             Getting work:\n\
             - Told a number (\"pick up task 42\") → `task_claim`.\n\
             - Told to work the queue, with no number → `task_next`. It picks the most \
             important task that is actually workable and claims it for you. Call it again \
             when you finish; keep going until it comes back with nothing.\n\
             \n\
             Working a task:\n\
             1. `task_get` to read the full instructions.\n\
             2. Claim BEFORE doing any work. Claims are atomic — if another agent holds the \
             task the call fails and tells you who has it; relay that to the human instead of \
             working the task anyway.\n\
             3. `task_scope` with the files you are about to change, as soon as you know \
             them. This is how the queue keeps two agents out of the same file: it answers \
             with any overlap with work already under way. If it reports one, say so and \
             coordinate rather than editing over someone.\n\
             4. `task_update` with a short note as you go. A claim is a lease of {ttl} minutes \
             and every update renews it, so call it at least every {heartbeat} minutes or the \
             task returns to the pool and another agent may take it.\n\
             5. `task_complete` with a result summary, or `task_fail` with a reason. \
             Only the lease holder may do either.\n\
             \n\
             When a task turns out to be bigger than one job, `task_split` files the pieces \
             as real tasks, makes the original wait for them, and puts it back in the pool — \
             so the other agents can work the pieces in parallel while it waits. When you \
             simply cannot do a task, `task_release` hands it back without marking it failed.\n\
             \n\
             Tasks can depend on other tasks. A task whose dependencies are unfinished \
             cannot be claimed, and the refusal names what it is waiting for.\n\
             \n\
             Some tasks are reviews, and a review is barred to the harness whose work it \
             reviews — including this one. If a claim comes back saying the task is recused, \
             that is not a race you can retry: relay it to the human, who needs to point a \
             different tool at it. When a task you finish was marked for review, the answer \
             names the review it filed; say that number out loud too. You are not being \
             distrusted, you are being read by somebody who was not in the room, which is \
             the whole reason the human is running more than one of us.\n\
             \n\
             Memory: `mem_store` durable facts you learn — where something lives, why a \
             decision was made, what a command is — one assertion per call, in plain prose \
             that will still make sense to a different agent next week. Pass `task_seq` when \
             you learned it working a task. `mem_search` before exploring from scratch; \
             another agent may already have found the answer.\n\
             \n\
             Recall: a claimed task comes back with `recalled` — facts earlier agents \
             recorded while working the same files, each with a `why` saying where it came \
             from. Read those before you start; they are the reason to declare your paths \
             early, because file scope is how the queue knows what to hand you. They are \
             assertions, not gospel: if one turns out to be wrong, `mem_store` the truth.\n\
             {footing}\
             {witness}\n\
             Everything is scoped to the current project ({project}) unless you pass \
             `all_projects: true`.",
            ttl = self.config.lease_ttl_minutes,
            heartbeat = self.heartbeat_minutes(),
            project = self.project,
            footing = self.footing_instructions(),
            witness = self.witness_instructions(),
        )
    }

    /// The paragraph about memory's footing, or nothing where there is none.
    ///
    /// Left out for the same reason as the witness paragraph: a model told to
    /// read a `standing` field that nothing will ever populate has been handed
    /// a rule it cannot use and a reason to doubt the ones it can.
    fn footing_instructions(&self) -> &'static str {
        if self.footing().is_none() {
            return "";
        }
        "\nHow much to trust a fact: assertions carry a `standing`, because hird records \
         which files each one was read off and what those files said at the time. `firm` \
         means the code has not moved since; `shaky` means it has, so the fact is \
         unverified rather than wrong — re-read the file before you act on it; `orphaned` \
         means the file is gone. Nothing marks a fact false; that is your call after \
         looking. When you check a shaky fact and it still holds, `mem_store` it again \
         word for word: that does not duplicate it, it re-anchors it to today's code and \
         records you as another voice for it, and the next agent gets it marked firm. When \
         it does not hold, `mem_store` the truth instead. Pass `paths` (or `task_seq`) so \
         what you record gets a footing of its own.\n"
    }

    /// The paragraph about the witness, or nothing at all where there is none.
    ///
    /// Left out entirely rather than hedged: a model told about `contended` in
    /// a project where nothing will ever populate it has been given a rule it
    /// cannot use and a reason to doubt the ones it can.
    fn witness_instructions(&self) -> &'static str {
        if self.witness.is_none() {
            return "";
        }
        "\nThe working tree: hird watches it. A claim fingerprints the repository, and \
         every task_update, task_scope and finishing call compares it again, so results \
         come back with `changed` — the files that moved while you held the task, off the \
         disk rather than out of anyone's summary. Two things there are worth acting on. \
         `undeclared` lists files you have changed that you never declared: call \
         task_scope with them, because the other agents' collision checks are reading \
         your declaration and not your edits. `contended` means a file you declared has \
         moved under another agent that declared it too — whoever read it first is \
         holding a copy that no longer matches, so re-read it before your next write and \
         tell the human. hird watched the file change; it cannot see who typed, so treat \
         `changed` as what happened in your window, not as a list of your own edits.\n"
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
pub struct TaskClaimArgs {
    /// The task number.
    pub seq: i64,
    /// Files you expect to change, as paths or globs relative to the project
    /// root, e.g. ["src/config.rs", "tests/**"]. Declaring them is how the
    /// queue can warn you that another agent is already in there.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskNextArgs {
    /// Consider tasks from every project rather than just the current one.
    #[serde(default)]
    pub all_projects: Option<bool>,
    /// Pass over tasks whose files overlap work another agent holds.
    /// On by default.
    #[serde(default)]
    pub avoid_conflicts: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskScopeArgs {
    /// The task number. You must hold its lease.
    pub seq: i64,
    /// Paths or globs relative to the project root, e.g. ["src/**/*.rs"].
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskSplitArgs {
    /// The task to break up. You must hold its lease.
    pub seq: i64,
    /// The pieces, in the order they should be worked.
    pub subtasks: Vec<SubtaskArgs>,
    /// Make each piece wait for the one before it. Leave this off when the
    /// pieces can be worked at the same time by different agents.
    #[serde(default)]
    pub sequential: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubtaskArgs {
    /// Short title, as a human would say it.
    pub title: String,
    /// Full instructions for whichever agent picks this up. Write it for
    /// someone who has not seen your session.
    #[serde(default)]
    pub body: Option<String>,
    /// Higher sorts first. Defaults to the parent task's priority.
    #[serde(default)]
    pub priority: Option<i64>,
    /// Files this piece is expected to touch.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskReleaseArgs {
    /// The task number. You must hold its lease.
    pub seq: i64,
    /// Why you are handing it back, for the next agent and the human.
    pub reason: String,
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
    /// The files this fact is about, as literal paths relative to the project
    /// root, e.g. ["src/config.rs"]. hird records what they say right now, so
    /// a later reader can be told if the code has moved under this fact.
    /// Leave it off when you passed `task_seq` — the files that task is in are
    /// used instead.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
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
    /// Unfinished tasks this one waits for. An `open` task with any of these
    /// cannot be claimed yet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocked_by: Vec<i64>,
    /// Set when this task is a review of work *you* did, so you cannot claim
    /// it however open it looks. Same reason `blocked_by` is here: a list that
    /// shows unclaimable work as available is lying to whoever reads it.
    #[serde(skip_serializing_if = "Option::is_none")]
    recused_from_you: Option<String>,
}

impl TaskRow {
    fn from_summary(
        summary: TaskSummary,
        show_project: bool,
        blocked_by: Vec<i64>,
        recused: Option<Recusal>,
    ) -> TaskRow {
        TaskRow {
            seq: summary.seq,
            title: summary.title,
            status: summary.status,
            holder: summary.claimed_by,
            priority: summary.priority,
            updated_at: summary.updated_at,
            project: show_project.then_some(summary.project),
            blocked_by,
            recused_from_you: recused.map(|r| r.describe()),
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
    /// Tasks that must finish before this one can be claimed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    waiting_for: Vec<BlockerRow>,
    /// Tasks that are waiting for this one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocks: Vec<i64>,
    /// The files this task has said it will touch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    /// Overlaps between those files and work other agents hold right now.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    overlaps: Vec<String>,
    /// Who cannot claim this task, and why. Present on reviews.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recused_from: Vec<String>,
    /// True when finishing this task will file a review of what it changed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    review_on_finish: bool,
    /// What earlier work in the same territory learned. Read before exploring.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recalled: Vec<RecallRow>,
    /// What the working tree was seen to do while this task was held, as
    /// against `paths`, which is what somebody said it would do.
    #[serde(flatten)]
    witness: Evidence,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<EventRow>,
}

/// A dependency, with enough context to act on without another lookup.
#[derive(Debug, Serialize)]
struct BlockerRow {
    seq: i64,
    title: String,
    status: Status,
}

impl From<Blocker> for BlockerRow {
    fn from(b: Blocker) -> BlockerRow {
        BlockerRow {
            seq: b.seq,
            title: b.title,
            status: b.status,
        }
    }
}

/// Everything about a task other than its own row.
struct TaskContext {
    waiting_for: Vec<Blocker>,
    blocks: Vec<i64>,
    paths: Vec<String>,
    conflicts: Vec<Conflict>,
    recusals: Vec<Recusal>,
    recalled: Vec<Recalled>,
    witness: Evidence,
    events: Vec<TaskEvent>,
}

impl TaskDetail {
    fn new(task: Task, context: TaskContext) -> TaskDetail {
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
            waiting_for: context
                .waiting_for
                .into_iter()
                .filter(|b| !b.is_cleared())
                .map(BlockerRow::from)
                .collect(),
            blocks: context.blocks,
            paths: context.paths,
            overlaps: describe_all(&context.conflicts),
            recused_from: context.recusals.iter().map(Recusal::describe).collect(),
            review_on_finish: task.review,
            recalled: recall_rows(context.recalled),
            witness: context.witness,
            events: context.events.into_iter().map(EventRow::from).collect(),
        }
    }
}

/// Conflicts as the sentences a model can relay without rewording them.
fn describe_all(conflicts: &[Conflict]) -> Vec<String> {
    conflicts.iter().map(Conflict::describe).collect()
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

/// One assertion the queue volunteered, and why.
///
/// `why` is a sentence rather than a code: the agent that reads this is a
/// language model, and "learned on task 4 (Port the config loader), working
/// src/config.rs" tells it how much to trust the fact and who to ask.
#[derive(Debug, Serialize)]
struct RecallRow {
    content: String,
    why: String,
    actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_seq: Option<i64>,
    created_at: String,
    /// Pass to `mem_search` results or quote to the human; assertions are
    /// never edited, so an id stays valid.
    id: String,
    /// `firm`, `shaky` or `orphaned`: whether the code this was learned from is
    /// still the code on disk. Absent means hird has no footing for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    standing: Option<&'static str>,
    /// Present only when there is something to do about it — the files have
    /// moved, or the agent that wrote this is not the only one who has said it.
    #[serde(skip_serializing_if = "Option::is_none")]
    caution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corroboration: Option<String>,
}

impl RecallRow {
    fn new(r: Recalled) -> RecallRow {
        let standing = r.standing.filter(|s| *s != Standing::Unanchored);
        RecallRow {
            why: r.reason.describe(),
            content: r.assertion.content,
            actor: r.assertion.actor,
            task_seq: r.task_seq,
            created_at: r.assertion.created_at,
            id: r.assertion.id,
            standing: standing.as_ref().map(|s| s.as_str()),
            // Only the standings worth acting on carry their sentence here. A
            // claim's recall is the smallest budget in hird and it lands in an
            // agent's context unasked; "this file is unchanged" spent on all
            // five rows would crowd out the one row that says otherwise.
            caution: standing
                .filter(Standing::needs_checking)
                .and_then(|s| s.describe()),
            corroboration: r.corroboration,
        }
    }
}

fn recall_rows(recalled: Vec<Recalled>) -> Vec<RecallRow> {
    recalled.into_iter().map(RecallRow::new).collect()
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
    /// The files this task is on record as touching.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    /// Overlaps with work other agents hold. Non-empty means someone else is
    /// already in these files.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    overlaps: Vec<String>,
    /// What earlier agents recorded about this task or these files. Nobody
    /// asked for it; the queue knew it was relevant and sent it along.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recalled: Vec<RecallRow>,
    /// Restated so the model does not have to remember the initialize text.
    reminder: String,
}

impl ClaimResult {
    fn new(claim: Claim, holder: String, heartbeat: u64, recalled: Vec<Recalled>) -> ClaimResult {
        let overlaps = describe_all(&claim.conflicts);
        let heartbeat_rule = format!(
            "call task_update at least every {heartbeat} minutes to keep this lease, \
             then task_complete or task_fail when you are done"
        );
        let mut reminder = if overlaps.is_empty() {
            heartbeat_rule
        } else {
            format!(
                "another agent is already in some of these files — say so before you edit \
                 them. Otherwise: {heartbeat_rule}"
            )
        };
        if !recalled.is_empty() {
            reminder = format!(
                "read `recalled` first — earlier agents left those notes about this work, \
                 and each says where it came from. Then: {reminder}"
            );
        }
        ClaimResult {
            claimed: claim.task.seq,
            holder,
            lease_expires_at: claim.task.lease_expires_at.unwrap_or_default(),
            title: claim.task.title,
            body: claim.task.body,
            priority: claim.task.priority,
            project: claim.task.project,
            paths: claim.paths,
            overlaps,
            recalled: recall_rows(recalled),
            reminder,
        }
    }
}

/// What `task_next` answers with: a claim, or the reason there wasn't one.
#[derive(Debug, Serialize)]
struct NextResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    claimed: Option<ClaimResult>,
    /// One sentence explaining an empty-handed answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    idle: Option<String>,
    /// Open tasks that are waiting on unfinished dependencies.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocked: Vec<i64>,
    /// Ready tasks passed over because another agent is in their files.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deferred: Vec<DeferredRow>,
    /// Ready tasks this harness is barred from, and why. Usually a review of
    /// work this harness did: say so to the human, because the task is waiting
    /// for a different tool and not for time to pass.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recused: Vec<RecusedRow>,
}

#[derive(Debug, Serialize)]
struct DeferredRow {
    seq: i64,
    overlaps: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RecusedRow {
    seq: i64,
    why: String,
}

impl NextResult {
    fn new(
        dispatch: Dispatch,
        holder: String,
        heartbeat: u64,
        recalled: Vec<Recalled>,
    ) -> NextResult {
        let idle = dispatch.claim.is_none().then(|| idle_reason(&dispatch));
        NextResult {
            claimed: dispatch
                .claim
                .map(|claim| ClaimResult::new(claim, holder, heartbeat, recalled)),
            idle,
            blocked: dispatch.blocked,
            deferred: dispatch
                .deferred
                .into_iter()
                .map(|(seq, conflicts)| DeferredRow {
                    seq,
                    overlaps: describe_all(&conflicts),
                })
                .collect(),
            recused: dispatch
                .recused
                .into_iter()
                .map(|(seq, recusal)| RecusedRow {
                    seq,
                    why: recusal.describe(),
                })
                .collect(),
        }
    }
}

/// Why the queue had nothing to hand out, in the terms the agent can act on.
fn idle_reason(dispatch: &Dispatch) -> String {
    // Named first when it is the only thing left, because it is the only one of
    // the three that waiting will not fix: a review of this harness's own work
    // stays unclaimable until somebody opens a different tool.
    let recused = dispatch.recused.len();
    if recused > 0 && dispatch.blocked.is_empty() && dispatch.deferred.is_empty() {
        return format!(
            "{recused} task{} ready, but every one of them is a review of work this harness \
             did — they need an agent in another harness. Tell the human; waiting will not \
             change it",
            plural(recused, " is", "s are")
        );
    }
    let tail = if recused > 0 {
        format!(", and {recused} that this harness is recused from (they need another harness)")
    } else {
        String::new()
    };
    match (dispatch.blocked.len(), dispatch.deferred.len()) {
        (0, 0) => format!("nothing is open in this project; the queue is empty{tail}"),
        (0, n) => format!(
            "{n} task{} ready, but every one of them touches files another agent is \
             working right now; try again once they finish{tail}",
            plural(n, " is", "s are")
        ),
        (n, 0) => format!(
            "{n} task{} open, but all of them are waiting on unfinished dependencies{tail}",
            plural(n, " is", "s are")
        ),
        (blocked, deferred) => format!(
            "nothing is workable: {blocked} task{} waiting on dependencies and \
             {deferred} overlapping files another agent is in{tail}",
            plural(blocked, " is", "s are")
        ),
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// What the witness saw, in the shape every tool that reports it uses.
///
/// Flattened into the results rather than nested, so an agent reading a
/// `task_update` reply sees `contended` at the top level next to its own note
/// instead of having to know there is a witness at all. Every field disappears
/// when it is empty, which is the ordinary case and the quiet one.
#[derive(Debug, Default, Serialize)]
struct Evidence {
    /// Files that moved while you held this task, as `path (modified)`. Not a
    /// claim about who moved them — one checkout, several agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    changed: Vec<String>,
    /// Files you and another live agent both declared, which have since moved.
    /// Somebody's copy is out of date; the sentence says whose and what to do.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    contended: Vec<String>,
    /// Files that moved under you that none of your declared patterns covers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    undeclared: Vec<String>,
    /// Set when the working tree was too dirty to fingerprint in full, so
    /// `changed` is a floor rather than the whole story. Reported rather than
    /// swallowed: a list that silently stops short reads exactly like a list
    /// that found everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    partial: Option<String>,
    /// Whether another agent held a task in this project at the same time.
    /// Not reported — it only decides how confidently the advice can speak.
    #[serde(skip)]
    alone: bool,
}

impl Evidence {
    /// The one thing to do about it, or nothing.
    ///
    /// Ordered by cost of ignoring it: losing another agent's work beats
    /// leaving the board's picture of your file scope wrong.
    fn advice(&self) -> Option<String> {
        if !self.contended.is_empty() {
            return Some(
                "a file you declared has changed under another agent that declared it too — \
                 re-read it before your next write, and tell the human"
                    .to_string(),
            );
        }
        if !self.undeclared.is_empty() {
            let listed = self.undeclared.join(", ");
            // Alone in the project, a file that moved moved because of you.
            // With another agent live, hird watched the file and not the
            // keyboard, and the advice has to leave room for that.
            return Some(if self.alone {
                format!(
                    "you have changed {} nobody was told about: {listed}. Call task_scope \
                     with {} so the other agents' collision checks can see it",
                    plural(self.undeclared.len(), "a file", "files"),
                    plural(self.undeclared.len(), "it", "them"),
                )
            } else {
                format!(
                    "{listed} moved while you held this task and nobody has declared {}. If \
                     the edit was yours, call task_scope so the other agents can see it; if \
                     it was not, another agent is working outside what it declared",
                    plural(self.undeclared.len(), "it", "them"),
                )
            });
        }
        None
    }
}

#[derive(Debug, Serialize)]
struct ScopeResult {
    seq: i64,
    paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    overlaps: Vec<String>,
    #[serde(flatten)]
    witness: Evidence,
    /// What to do about it, in one sentence.
    advice: String,
}

#[derive(Debug, Serialize)]
struct SplitResult {
    seq: i64,
    status: Status,
    /// The pieces, in the order they were filed.
    subtasks: Vec<SubtaskRow>,
    note: String,
}

#[derive(Debug, Serialize)]
struct SubtaskRow {
    seq: i64,
    title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    waiting_for: Vec<i64>,
}

#[derive(Debug, Serialize)]
struct UpdateResult {
    seq: i64,
    status: Status,
    lease_expires_at: String,
    note_recorded: String,
    #[serde(flatten)]
    witness: Evidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    advice: Option<String>,
}

#[derive(Debug, Serialize)]
struct FinishResult {
    seq: i64,
    status: Status,
    result: String,
    /// What the working tree says this task did, which is not the same source
    /// as `result` — that is the agent's own account of itself.
    #[serde(flatten)]
    witness: Evidence,
    /// The review task this completion filed, if the work was marked for one.
    /// It is scoped to the files that moved and recused from your harness, so
    /// another agent will pick it up. Tell the human its number.
    #[serde(skip_serializing_if = "Option::is_none")]
    review_filed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    advice: Option<String>,
}

impl FinishResult {
    fn new(
        seq: i64,
        status: Status,
        result: String,
        witness: Evidence,
        review_filed: Option<i64>,
    ) -> FinishResult {
        // A filed review outranks the witness's advice: it is the thing the
        // agent has to say out loud, and it is about to stop being this
        // agent's business entirely.
        let advice = match review_filed {
            Some(review) => Some(format!(
                "this work was marked for review, so task {review} is now open for an agent in \
                 another harness — you cannot take it yourself. Tell the human."
            )),
            None => witness.advice(),
        };
        FinishResult {
            seq,
            status,
            result,
            advice,
            witness,
            review_filed,
        }
    }
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
    /// True when this exact sentence was already on file. Nothing was
    /// duplicated: the original assertion keeps its id and its author, you have
    /// been recorded as another voice for it, and its footing has been re-taken
    /// against the code as it stands now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    affirmed: bool,
    /// The files this fact is now on record as having been read off.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    anchored_to: Vec<String>,
    /// Who has stated it, if more than one agent has.
    #[serde(skip_serializing_if = "Option::is_none")]
    corroboration: Option<String>,
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
    /// `firm`, `shaky` or `orphaned` — how the files this was learned against
    /// compare with the ones on disk right now. Absent means hird has no
    /// footing for it and is not guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    standing: Option<&'static str>,
    /// The same, as a sentence you can relay.
    #[serde(skip_serializing_if = "Option::is_none")]
    footing: Option<String>,
}

impl AssertionRow {
    fn new(a: Assertion, show_project: bool, standing: Option<&Standing>) -> AssertionRow {
        let tags = a.tag_list().into_iter().map(str::to_string).collect();
        let standing = standing.filter(|s| **s != Standing::Unanchored);
        AssertionRow {
            id: a.id,
            tags,
            content: a.content,
            actor: a.actor,
            created_at: a.created_at,
            project: show_project.then_some(a.project),
            superseded: a.superseded_by.is_some(),
            standing: standing.map(Standing::as_str),
            footing: standing.and_then(Standing::describe),
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

        let actor = self.actor();
        let (released, tasks, unmet, recused) = self.with_db(|db| {
            let released = db
                .tasks()
                .sweep_leases()
                .map(|s| s.expired)
                .unwrap_or_default();
            let listed = db.tasks().list(&scope, status);
            // Which of these this session is barred from. Only the open ones
            // are worth asking about: a claimed or finished task's claimability
            // is not the reader's question.
            let recused: BTreeMap<i64, Recusal> = listed
                .as_ref()
                .map(|tasks| {
                    tasks
                        .iter()
                        .filter(|t| t.status == Status::Open)
                        .filter_map(|t| {
                            db.recusals()
                                .bars(t.seq, &actor)
                                .ok()
                                .flatten()
                                .map(|r| (t.seq, r))
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                released,
                listed,
                db.deps().unmet_map(&scope).unwrap_or_default(),
                recused,
            )
        });
        let tasks = tasks.map_err(stringify)?;

        json(&TaskListResult {
            project: &self.project,
            all_projects: scope.is_all(),
            count: tasks.len(),
            tasks: tasks
                .into_iter()
                .map(|t| {
                    let blocked = unmet.get(&t.seq).cloned().unwrap_or_default();
                    let barred = recused.get(&t.seq).cloned();
                    TaskRow::from_summary(t, show_project, blocked, barred)
                })
                .collect(),
            released,
        })
    }

    /// Read one task in full: instructions, dependencies, file scope and history.
    #[tool(name = "task_get")]
    async fn task_get(&self, Parameters(args): Parameters<SeqArgs>) -> Result<String, String> {
        let recall_limit = self.config.recall_limit();
        let detail = self.with_db(|db| {
            let task = db.tasks().get(args.seq)?;
            let (waiting_for, conflicts) = db.tasks().readiness(args.seq)?;
            Ok::<_, Error>(TaskDetail::new(
                task.clone(),
                TaskContext {
                    waiting_for,
                    blocks: db
                        .deps()
                        .dependents(args.seq)?
                        .into_iter()
                        .map(|b| b.seq)
                        .collect(),
                    paths: db.scopes().for_task(args.seq)?,
                    conflicts,
                    recusals: db.recusals().for_task(args.seq)?,
                    recalled: self.recall(db, args.seq, recall_limit),
                    witness: self.evidence(db, args.seq),
                    events: db.tasks().events(&task.id, EVENT_WINDOW)?,
                },
            ))
        });
        json(&detail.map_err(stringify)?)
    }

    /// Claim a task before working on it. Fails if someone else already holds
    /// it, or if it is still waiting on another task.
    #[tool(name = "task_claim")]
    async fn task_claim(
        &self,
        Parameters(args): Parameters<TaskClaimArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let ttl = self.config.lease_ttl();
        let paths = args.paths.unwrap_or_default();
        let policy = self.config.on_conflict();
        let recall_limit = self.config.recall_limit();
        // Recall runs after the claim, so it sees the paths this call just
        // declared — claiming with `paths` is what makes the file-scope half
        // of recall work on the very first call.
        let (claim, recalled) = self
            .with_db(|db| {
                let claim = db
                    .tasks()
                    .claim_scoped(args.seq, &actor, ttl, &paths, policy)?;
                // The tree as it stands is this task's baseline, so anything
                // that moves from here is inside its window and everything
                // already dirty is not.
                self.begin_witnessing(db, args.seq);
                Ok::<_, Error>((claim, self.recall(db, args.seq, recall_limit)))
            })
            .map_err(stringify)?;

        json(&ClaimResult::new(
            claim,
            actor,
            self.heartbeat_minutes(),
            recalled,
        ))
    }

    /// Be handed the next task worth doing, already claimed. Use this when the
    /// human says to work the queue without naming a task number.
    #[tool(name = "task_next")]
    async fn task_next(
        &self,
        Parameters(args): Parameters<TaskNextArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let ttl = self.config.lease_ttl();
        let scope = self.scope(args.all_projects);
        let avoid = self.config.avoid_conflicts(args.avoid_conflicts);
        let recall_limit = self.config.recall_limit();
        let (dispatch, recalled) = self
            .with_db(|db| {
                let dispatch = db.tasks().claim_next(&actor, ttl, &scope, avoid)?;
                let recalled = match &dispatch.claim {
                    Some(claim) => {
                        self.begin_witnessing(db, claim.task.seq);
                        self.recall(db, claim.task.seq, recall_limit)
                    }
                    None => Vec::new(),
                };
                Ok::<_, Error>((dispatch, recalled))
            })
            .map_err(stringify)?;

        json(&NextResult::new(
            dispatch,
            actor,
            self.heartbeat_minutes(),
            recalled,
        ))
    }

    /// Say which files a task you hold is going to change, and find out
    /// whether another agent is already working in them.
    #[tool(name = "task_scope")]
    async fn task_scope(
        &self,
        Parameters(args): Parameters<TaskScopeArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let policy = self.config.on_conflict();
        let (paths, conflicts, evidence) = self
            .with_db(|db| {
                // Holder-only, like every other write to a claimed task.
                let task = db.tasks().get(args.seq)?;
                if !task.status.is_active() || task.claimed_by.as_deref() != Some(actor.as_str()) {
                    return Err(Error::NotHolder {
                        seq: args.seq,
                        status: task.status,
                        holder: task.claimed_by,
                        actor: actor.clone(),
                    });
                }
                let conflicts = db.scopes().declare(args.seq, &args.paths, &actor, policy)?;
                // Declaring is the moment the witness has something to say:
                // these patterns are what the contention detector needs, and
                // the tree may already have moved under them.
                Ok::<_, Error>((
                    db.scopes().for_task(args.seq)?,
                    conflicts,
                    self.witnessed(db, args.seq),
                ))
            })
            .map_err(stringify)?;

        let overlaps = describe_all(&conflicts);
        let advice = match (evidence.advice(), overlaps.is_empty()) {
            (Some(urgent), _) => urgent,
            (None, false) => "tell the human about the overlap before editing those files, \
                              and prefer to work elsewhere until it clears"
                .to_string(),
            (None, true) => "no other agent is in these files; go ahead".to_string(),
        };
        json(&ScopeResult {
            seq: args.seq,
            paths,
            overlaps,
            witness: evidence,
            advice,
        })
    }

    /// Break a task you hold into pieces other agents can work in parallel.
    /// The original waits for them and goes back in the pool.
    #[tool(name = "task_split")]
    async fn task_split(
        &self,
        Parameters(args): Parameters<TaskSplitArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let empty: Vec<String> = Vec::new();
        let subtasks: Vec<Subtask<'_>> = args
            .subtasks
            .iter()
            .map(|s| Subtask {
                title: &s.title,
                body: s.body.as_deref().unwrap_or(""),
                priority: s.priority,
                paths: s.paths.as_ref().unwrap_or(&empty),
            })
            .collect();
        let sequential = args.sequential.unwrap_or(false);

        let (parent, children) = self
            .with_db(|db| db.tasks().split(args.seq, &actor, &subtasks, sequential))
            .map_err(stringify)?;

        let mut previous: Option<i64> = None;
        let rows = children
            .iter()
            .map(|child| {
                let waiting_for = match previous.replace(child.seq) {
                    Some(prior) if sequential => vec![prior],
                    _ => Vec::new(),
                };
                SubtaskRow {
                    seq: child.seq,
                    title: child.title.clone(),
                    waiting_for,
                }
            })
            .collect();

        json(&SplitResult {
            seq: parent.seq,
            status: parent.status,
            subtasks: rows,
            note: format!(
                "task {} is back in the pool and now waits for {} piece{}; you no longer \
                 hold it. Call task_next for your own next piece of work",
                parent.seq,
                children.len(),
                if children.len() == 1 { "" } else { "s" }
            ),
        })
    }

    /// Hand back a task you hold without finishing it, so another agent can
    /// take it. Prefer task_fail when the task itself is the problem.
    #[tool(name = "task_release")]
    async fn task_release(
        &self,
        Parameters(args): Parameters<TaskReleaseArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let (task, evidence) = self
            .with_db(|db| {
                // Before the release, while this session still holds the lease
                // and the record of what moved under it is still its own.
                let evidence = self.witnessed(db, args.seq);
                let task = db.tasks().release(args.seq, &actor, &args.reason)?;
                Ok::<_, Error>((task, evidence))
            })
            .map_err(stringify)?;
        json(&FinishResult::new(
            task.seq,
            task.status,
            args.reason.trim().to_string(),
            evidence,
            // Releasing is not finishing, so nothing is filed to review.
            None,
        ))
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
        let (task, evidence) = self
            .with_db(|db| {
                let task = db
                    .tasks()
                    .update(args.seq, &actor, start, &args.note, ttl)?;
                // The heartbeat is the natural place to look: the agent is
                // between edits, and this is the last chance to tell it a file
                // moved before it writes the next one.
                Ok::<_, Error>((task, self.witnessed(db, args.seq)))
            })
            .map_err(stringify)?;

        json(&UpdateResult {
            seq: task.seq,
            status: task.status,
            lease_expires_at: task.lease_expires_at.unwrap_or_default(),
            note_recorded: args.note.trim().to_string(),
            advice: evidence.advice(),
            witness: evidence,
        })
    }

    /// Finish a task you hold, with a summary of what was done.
    #[tool(name = "task_complete")]
    async fn task_complete(
        &self,
        Parameters(args): Parameters<TaskCompleteArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let (task, evidence) = self
            .with_db(|db| {
                // Last look, taken while the task is still live so its
                // footprint is complete and any contention is still true.
                let evidence = self.witnessed(db, args.seq);
                // And the last word on what this task learned: its facts are
                // statements about the tree it is leaving behind, not the one
                // it found halfway through its own edits.
                footing::settle(db, self.footing(), args.seq);
                let task = db.tasks().complete(args.seq, &actor, &args.result)?;
                Ok::<_, Error>((task, evidence))
            })
            .map_err(stringify)?;
        json(&FinishResult::new(
            task.task.seq,
            task.task.status,
            task.task.result.unwrap_or_default(),
            evidence,
            task.review,
        ))
    }

    /// Give up on a task you hold, saying why. The human can reopen it later.
    #[tool(name = "task_fail")]
    async fn task_fail(
        &self,
        Parameters(args): Parameters<TaskFailArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let (task, evidence) = self
            .with_db(|db| {
                let evidence = self.witnessed(db, args.seq);
                footing::settle(db, self.footing(), args.seq);
                let task = db.tasks().fail(args.seq, &actor, &args.reason)?;
                Ok::<_, Error>((task, evidence))
            })
            .map_err(stringify)?;
        json(&FinishResult::new(
            task.task.seq,
            task.task.status,
            task.task.result.unwrap_or_default(),
            evidence,
            task.review,
        ))
    }

    /// Record one durable fact so other agents and sessions can find it later.
    #[tool(name = "mem_store")]
    async fn mem_store(
        &self,
        Parameters(args): Parameters<MemStoreArgs>,
    ) -> Result<String, String> {
        let actor = self.actor();
        let (recorded, anchored, corroboration) = self
            .with_db(|db| {
                let recorded = db.memory().record(NewAssertion {
                    project: &self.project,
                    content: &args.content,
                    tags: args.tags.as_deref().unwrap_or(""),
                    actor: &actor,
                    task_seq: args.task_seq,
                })?;
                // Anchoring runs on both paths, and that is the whole
                // re-grounding loop: an agent that rediscovers a fact says it
                // again, and saying it again is what re-takes its footing
                // against the code as it stands today.
                let ground = footing::ground(db, args.task_seq, args.paths.as_deref());
                let anchored =
                    footing::anchor(db, self.footing(), &recorded.assertion().id, &ground)
                        .unwrap_or_default();
                let corroboration = footing::corroboration(db, recorded.assertion());
                Ok::<_, Error>((recorded, anchored, corroboration))
            })
            .map_err(stringify)?;

        let affirmed = recorded.was_affirmed();
        let assertion = recorded.into_assertion();
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
            affirmed,
            anchored_to: anchored.into_iter().map(|a| a.path).collect(),
            corroboration,
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

        let (hits, standings) = self
            .with_db(|db| {
                let hits = db.memory().search(&query)?;
                let ids: Vec<String> = hits.iter().map(|a| a.id.clone()).collect();
                let standings = footing::standings(db, self.footing(), &self.project, &ids);
                Ok::<_, Error>((hits, standings))
            })
            .map_err(stringify)?;

        json(&MemSearchResult {
            query: &args.query,
            project: &self.project,
            all_projects: scope.is_all(),
            count: hits.len(),
            assertions: hits
                .into_iter()
                .map(|a| {
                    let standing = standings.get(&a.id).cloned();
                    AssertionRow::new(a, show_project, standing.as_ref())
                })
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

    /// A claim with no declared file scope, which is most of these tests.
    fn just(seq: i64) -> TaskClaimArgs {
        TaskClaimArgs { seq, paths: None }
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

        let out = parse(s.task_claim(Parameters(just(seq))).await);
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

        let err = s.task_claim(Parameters(just(seq))).await.unwrap_err();
        assert!(
            err.starts_with("task 1 is claimed by codex:9f2c until "),
            "{err}"
        );
    }

    #[tokio::test]
    async fn update_starts_work_and_renews_the_lease() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.task_claim(Parameters(just(seq))).await.unwrap();

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
        s.task_claim(Parameters(just(seq))).await.unwrap();

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
        s.task_claim(Parameters(just(done))).await.unwrap();
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
        s.task_claim(Parameters(just(bad))).await.unwrap();
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
                paths: None,
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
                paths: None,
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
            paths: None,
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
                paths: None,
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

    // ------------------------------------------------- dispatch, scope, split

    fn claim_with(seq: i64, paths: &[&str]) -> TaskClaimArgs {
        TaskClaimArgs {
            seq,
            paths: Some(paths.iter().map(|p| p.to_string()).collect()),
        }
    }

    fn next_args() -> TaskNextArgs {
        TaskNextArgs {
            all_projects: None,
            avoid_conflicts: None,
        }
    }

    #[tokio::test]
    async fn next_claims_the_best_available_task_and_reads_like_a_claim() {
        let s = server();
        seed(&s, "low priority", "");
        let important = s
            .with_db(|db| db.tasks().create(PROJECT, "important", "do it", 5, "cli"))
            .unwrap()
            .seq;

        let out = parse(s.task_next(Parameters(next_args())).await);
        assert_eq!(out["claimed"]["claimed"], important);
        assert_eq!(out["claimed"]["body"], "do it");
        assert_eq!(out["claimed"]["holder"], "claude-code:af31");
        assert!(out.get("idle").is_none());
    }

    #[tokio::test]
    async fn next_explains_an_empty_queue_rather_than_going_quiet() {
        let s = server();
        let out = parse(s.task_next(Parameters(next_args())).await);
        assert!(out.get("claimed").is_none());
        assert_eq!(
            out["idle"],
            "nothing is open in this project; the queue is empty"
        );
    }

    #[tokio::test]
    async fn next_names_the_dependencies_that_left_it_empty_handed() {
        let s = server();
        let gate = seed(&s, "gate", "");
        let waiting = seed(&s, "waiting", "");
        s.with_db(|db| db.deps().add(waiting, gate, "cli")).unwrap();
        // Someone else is already on the only unblocked task.
        s.with_db(|db| {
            db.tasks()
                .claim(gate, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        let out = parse(s.task_next(Parameters(next_args())).await);
        assert!(out.get("claimed").is_none());
        assert_eq!(out["blocked"], serde_json::json!([waiting]));
        assert!(
            out["idle"]
                .as_str()
                .unwrap()
                .contains("waiting on unfinished dependencies"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn next_reports_the_files_that_made_it_hold_back() {
        let s = server();
        let held = seed(&s, "held", "");
        let overlapping = seed(&s, "overlapping", "");
        s.with_db(|db| {
            db.scopes().declare(
                held,
                &["src/**".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )?;
            db.scopes().declare(
                overlapping,
                &["src/db.rs".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )?;
            db.tasks()
                .claim(held, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        let out = parse(s.task_next(Parameters(next_args())).await);
        assert!(out.get("claimed").is_none());
        assert_eq!(out["deferred"][0]["seq"], overlapping);
        let overlap = out["deferred"][0]["overlaps"][0].as_str().unwrap();
        assert!(overlap.contains("src/db.rs overlaps src/**"), "{overlap}");
        assert!(overlap.contains("codex:9f2c"), "{overlap}");
    }

    #[tokio::test]
    async fn a_blocked_task_cannot_be_claimed_and_the_error_says_what_to_wait_for() {
        let s = server();
        let gate = seed(&s, "write the schema", "");
        let waiting = seed(&s, "write the api", "");
        s.with_db(|db| db.deps().add(waiting, gate, "cli")).unwrap();

        let err = s.task_claim(Parameters(just(waiting))).await.unwrap_err();
        assert!(err.contains("task 2 is blocked by task 1"), "{err}");
        assert!(err.contains("write the schema"), "{err}");
    }

    #[tokio::test]
    async fn claiming_with_paths_records_them_and_warns_about_overlaps() {
        let s = server();
        let held = seed(&s, "held", "");
        let mine = seed(&s, "mine", "");
        s.with_db(|db| {
            db.scopes().declare(
                held,
                &["src/**".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )?;
            db.tasks()
                .claim(held, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        let out = parse(
            s.task_claim(Parameters(claim_with(mine, &["src/db.rs"])))
                .await,
        );
        assert_eq!(out["paths"], serde_json::json!(["src/db.rs"]));
        assert!(out["overlaps"][0].as_str().unwrap().contains("codex:9f2c"));
        // The reminder changes shape so the model cannot skim past a collision.
        assert!(out["reminder"]
            .as_str()
            .unwrap()
            .contains("already in some of these files"));
    }

    /// Seed a finished piece of work in `paths` that left a fact behind.
    fn earlier_work(s: &HirdMcp, title: &str, paths: &[&str], learned: &str) -> i64 {
        let seq = seed(s, title, "");
        s.with_db(|db| {
            let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            db.scopes()
                .declare(seq, &owned, "cli", crate::repo::OnConflict::Report)?;
            db.memory().store(NewAssertion {
                project: PROJECT,
                content: learned,
                tags: "",
                actor: "codex:9f2c",
                task_seq: Some(seq),
            })?;
            // Done and gone: recall reaches back through finished work, which
            // is the only kind that has anything to teach.
            db.tasks()
                .claim(seq, "codex:9f2c", Config::default().lease_ttl())?;
            db.tasks().complete(seq, "codex:9f2c", "done")
        })
        .unwrap();
        seq
    }

    /// Claiming with a file scope is enough to be told what the last agent in
    /// those files learned — without anyone calling `mem_search`.
    #[tokio::test]
    async fn a_claim_arrives_with_what_earlier_work_in_those_files_learned() {
        let s = server();
        let earlier = earlier_work(
            &s,
            "Port the config loader",
            &["src/config.rs"],
            "env vars beat the config file",
        );
        let mine = seed(&s, "Audit the loader", "");

        let out = parse(
            s.task_claim(Parameters(claim_with(mine, &["src/*.rs"])))
                .await,
        );
        let recalled = &out["recalled"][0];
        assert_eq!(recalled["content"], "env vars beat the config file");
        assert_eq!(recalled["task_seq"], earlier);
        assert_eq!(recalled["actor"], "codex:9f2c");
        let why = recalled["why"].as_str().unwrap();
        assert!(why.contains(&format!("task {earlier}")), "{why}");
        assert!(why.contains("src/config.rs"), "{why}");
        // And the model is told to read it before anything else.
        assert!(out["reminder"]
            .as_str()
            .unwrap()
            .starts_with("read `recalled` first"));
    }

    #[tokio::test]
    async fn self_dispatch_carries_the_same_recall_as_a_named_claim() {
        let s = server();
        earlier_work(
            &s,
            "Rewrite the renderer",
            &["src/tui/**"],
            "the renderer redraws on every poll",
        );
        let mine = seed(&s, "Speed up the renderer", "");
        s.with_db(|db| {
            db.scopes().declare(
                mine,
                &["src/tui/view.rs".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )
        })
        .unwrap();

        let out = parse(s.task_next(Parameters(next_args())).await);
        assert_eq!(out["claimed"]["claimed"], mine);
        assert_eq!(
            out["claimed"]["recalled"][0]["content"],
            "the renderer redraws on every poll"
        );
    }

    /// Recall is a courtesy, not a promise: a task nothing relates to simply
    /// leaves the field out rather than sending an empty list.
    #[tokio::test]
    async fn a_claim_with_nothing_to_recall_says_nothing() {
        let s = server();
        let seq = seed(&s, "Xyzzy", "");
        let out = parse(s.task_claim(Parameters(just(seq))).await);
        assert!(out.get("recalled").is_none(), "{out}");
        assert!(out["reminder"]
            .as_str()
            .unwrap()
            .starts_with("call task_update"));
    }

    #[tokio::test]
    async fn task_get_shows_the_same_recall_without_claiming_anything() {
        let s = server();
        earlier_work(
            &s,
            "Port the config loader",
            &["src/config.rs"],
            "env vars beat the config file",
        );
        let mine = seed(&s, "Audit the loader", "");
        s.with_db(|db| {
            db.scopes().declare(
                mine,
                &["src/config.rs".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )
        })
        .unwrap();

        let out = parse(s.task_get(Parameters(SeqArgs { seq: mine })).await);
        assert_eq!(
            out["recalled"][0]["content"],
            "env vars beat the config file"
        );
        assert_eq!(out["status"], "open");
    }

    #[tokio::test]
    async fn recall_can_be_switched_off_in_the_configuration() {
        let s = HirdMcp::new(
            Db::open_in_memory().unwrap(),
            AgentId::new("claude-code", "af31"),
            PROJECT.to_string(),
            Config {
                recall_limit: 0,
                ..Config::default()
            },
        );
        earlier_work(&s, "Port the loader", &["src/config.rs"], "a fact");
        let mine = seed(&s, "Audit the loader", "");
        let out = parse(
            s.task_claim(Parameters(claim_with(mine, &["src/config.rs"])))
                .await,
        );
        assert!(out.get("recalled").is_none(), "{out}");
    }

    #[tokio::test]
    async fn scope_is_holder_only_and_answers_with_advice() {
        let s = server();
        let seq = seed(&s, "t", "");

        let err = s
            .task_scope(Parameters(TaskScopeArgs {
                seq,
                paths: vec!["src/lib.rs".into()],
            }))
            .await
            .unwrap_err();
        assert!(err.contains("must claim it first"), "{err}");

        s.task_claim(Parameters(just(seq))).await.unwrap();
        let out = parse(
            s.task_scope(Parameters(TaskScopeArgs {
                seq,
                paths: vec!["./src/lib.rs".into(), "tests/".into()],
            }))
            .await,
        );
        assert_eq!(out["paths"], serde_json::json!(["src/lib.rs", "tests/**"]));
        assert!(out.get("overlaps").is_none());
        assert!(out["advice"].as_str().unwrap().contains("go ahead"));
    }

    #[tokio::test]
    async fn splitting_files_the_pieces_and_hands_the_parent_back() {
        let s = server();
        let seq = seed(&s, "port the loader", "big job");
        s.task_claim(Parameters(just(seq))).await.unwrap();

        let out = parse(
            s.task_split(Parameters(TaskSplitArgs {
                seq,
                sequential: None,
                subtasks: vec![
                    SubtaskArgs {
                        title: "extract the parser".into(),
                        body: Some("start with the lexer".into()),
                        priority: None,
                        paths: Some(vec!["src/parse.rs".into()]),
                    },
                    SubtaskArgs {
                        title: "port the tests".into(),
                        body: None,
                        priority: Some(3),
                        paths: None,
                    },
                ],
            }))
            .await,
        );

        assert_eq!(out["seq"], seq);
        assert_eq!(out["status"], "open", "the parent goes back in the pool");
        assert_eq!(out["subtasks"][0]["title"], "extract the parser");
        assert_eq!(out["subtasks"][1]["seq"], 3);
        assert!(out["subtasks"][0].get("waiting_for").is_none());

        // The parent is now blocked by both pieces, so nobody can take it.
        let err = s.task_claim(Parameters(just(seq))).await.unwrap_err();
        assert!(err.contains("is blocked by task 2"), "{err}");

        // And the pieces are ordinary tasks any agent can be handed.
        let handed = parse(s.task_next(Parameters(next_args())).await);
        assert_eq!(handed["claimed"]["claimed"], 3, "priority 3 goes first");
    }

    #[tokio::test]
    async fn a_sequential_split_queues_the_pieces_behind_each_other() {
        let s = server();
        let seq = seed(&s, "migrate", "");
        s.task_claim(Parameters(just(seq))).await.unwrap();

        let out = parse(
            s.task_split(Parameters(TaskSplitArgs {
                seq,
                sequential: Some(true),
                subtasks: vec![
                    SubtaskArgs {
                        title: "write the migration".into(),
                        body: None,
                        priority: None,
                        paths: None,
                    },
                    SubtaskArgs {
                        title: "backfill".into(),
                        body: None,
                        priority: None,
                        paths: None,
                    },
                ],
            }))
            .await,
        );
        assert_eq!(out["subtasks"][1]["waiting_for"], serde_json::json!([2]));

        // Only the first piece is workable.
        let handed = parse(s.task_next(Parameters(next_args())).await);
        assert_eq!(handed["claimed"]["claimed"], 2);
        let nothing = parse(s.task_next(Parameters(next_args())).await);
        assert!(nothing.get("claimed").is_none());
        assert_eq!(nothing["blocked"], serde_json::json!([1, 3]));
    }

    #[tokio::test]
    async fn splitting_is_holder_only_and_needs_at_least_one_piece() {
        let s = server();
        let seq = seed(&s, "t", "");
        let err = s
            .task_split(Parameters(TaskSplitArgs {
                seq,
                sequential: None,
                subtasks: vec![SubtaskArgs {
                    title: "a piece".into(),
                    body: None,
                    priority: None,
                    paths: None,
                }],
            }))
            .await
            .unwrap_err();
        assert!(err.contains("must claim it first"), "{err}");

        s.task_claim(Parameters(just(seq))).await.unwrap();
        let err = s
            .task_split(Parameters(TaskSplitArgs {
                seq,
                sequential: None,
                subtasks: vec![],
            }))
            .await
            .unwrap_err();
        assert!(err.contains("at least one subtask"), "{err}");
    }

    #[tokio::test]
    async fn releasing_puts_a_task_back_without_marking_it_failed() {
        let s = server();
        let seq = seed(&s, "t", "");
        s.task_claim(Parameters(just(seq))).await.unwrap();

        let out = parse(
            s.task_release(Parameters(TaskReleaseArgs {
                seq,
                reason: "needs credentials I do not have".into(),
            }))
            .await,
        );
        assert_eq!(out["status"], "open");
        assert_eq!(out["result"], "needs credentials I do not have");

        // Straight back on the ready list, no human intervention needed.
        let handed = parse(s.task_next(Parameters(next_args())).await);
        assert_eq!(handed["claimed"]["claimed"], seq);
    }

    #[tokio::test]
    async fn task_get_shows_dependencies_scope_and_overlaps() {
        let s = server();
        let gate = seed(&s, "gate", "");
        let held = seed(&s, "held", "");
        let mine = seed(&s, "mine", "");
        s.with_db(|db| {
            db.deps().add(mine, gate, "cli")?;
            db.scopes().declare(
                held,
                &["src/**".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )?;
            db.scopes().declare(
                mine,
                &["src/db.rs".into()],
                "cli",
                crate::repo::OnConflict::Report,
            )?;
            db.tasks()
                .claim(held, "codex:9f2c", Config::default().lease_ttl())
        })
        .unwrap();

        let out = parse(s.task_get(Parameters(SeqArgs { seq: mine })).await);
        assert_eq!(out["waiting_for"][0]["seq"], gate);
        assert_eq!(out["waiting_for"][0]["status"], "open");
        assert_eq!(out["paths"], serde_json::json!(["src/db.rs"]));
        assert!(out["overlaps"][0].as_str().unwrap().contains("codex:9f2c"));

        let gate_view = parse(s.task_get(Parameters(SeqArgs { seq: gate })).await);
        assert_eq!(gate_view["blocks"], serde_json::json!([mine]));
    }

    #[tokio::test]
    async fn task_list_marks_the_tasks_nobody_can_start_yet() {
        let s = server();
        let gate = seed(&s, "gate", "");
        let waiting = seed(&s, "waiting", "");
        s.with_db(|db| db.deps().add(waiting, gate, "cli")).unwrap();

        let out = parse(
            s.task_list(Parameters(TaskListArgs {
                status: None,
                all_projects: None,
            }))
            .await,
        );
        let rows = out["tasks"].as_array().unwrap();
        let blocked = rows.iter().find(|r| r["seq"] == waiting).unwrap();
        assert_eq!(blocked["blocked_by"], serde_json::json!([gate]));
        let ready = rows.iter().find(|r| r["seq"] == gate).unwrap();
        assert!(ready.get("blocked_by").is_none());
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

    /// The tool surface is a promise to every harness that has registered
    /// `hird`, so it changes deliberately or not at all.
    #[test]
    fn exactly_the_designed_tools_are_exposed() {
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
                "task_next",
                "task_release",
                "task_scope",
                "task_split",
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
