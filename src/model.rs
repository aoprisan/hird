//! Core domain types: task status machine, tasks, events and assertions.
//!
//! The status machine in [`Status::apply`] is the single authority for which
//! task transitions exist; the repository layer refuses anything it rejects.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Format a timestamp the way every column in the database stores it.
///
/// Fixed-width RFC 3339 in UTC with millisecond precision, so lexicographic
/// ordering matches chronological ordering and `lease_expires_at < :now`
/// comparisons work as plain string comparisons in SQL.
pub fn fmt_ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The current time, formatted for storage.
pub fn now_ts() -> String {
    fmt_ts(Utc::now())
}

/// Parse a timestamp previously written by [`fmt_ts`].
pub fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Where a task sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    Claimed,
    InProgress,
    Done,
    Failed,
    Cancelled,
}

impl Status {
    /// Every status, for exhaustive iteration in tests and TUI filters.
    pub const ALL: [Status; 6] = [
        Status::Open,
        Status::Claimed,
        Status::InProgress,
        Status::Done,
        Status::Failed,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Claimed => "claimed",
            Status::InProgress => "in_progress",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    /// Terminal statuses can only leave via `reopen` — a human's, or the one a
    /// review's `sent_back` verdict performs on the work it judged.
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Cancelled)
    }

    /// Statuses that hold a lease.
    pub fn is_active(self) -> bool {
        matches!(self, Status::Claimed | Status::InProgress)
    }

    /// Apply a transition, returning the resulting status.
    ///
    /// `None` means the transition is not part of the status machine and must
    /// be rejected. This is the whole diagram, in one place:
    ///
    /// ```text
    /// open ──claim──► claimed ──start──► in_progress ──complete──► done
    ///   ▲                │                    │        └─fail────► failed
    ///   │                └── lease expiry ────┘
    ///   │                └──── release ───────┘
    ///   └── reopen (human, or a review's sent_back) ◄── done|failed|cancelled
    /// open ──cancel (human)──► cancelled
    /// ```
    ///
    /// `release` is the agent-side counterpart of a lease expiry: an agent
    /// that cannot finish a task hands it straight back rather than making
    /// everyone wait out the lease, and without the `failed` state a human
    /// would have to clear by hand.
    pub fn apply(self, transition: Transition) -> Option<Status> {
        use Status::*;
        use Transition::*;
        match (self, transition) {
            (Open, Claim) => Some(Claimed),
            // `start` is idempotent so an agent may repeat `task_update
            // status=in_progress` purely to renew its lease.
            (Claimed, Start) | (InProgress, Start) => Some(InProgress),
            (Claimed | InProgress, Complete) => Some(Done),
            (Claimed | InProgress, Fail) => Some(Failed),
            (Claimed | InProgress, LeaseExpiry | Release) => Some(Open),
            (Open | Claimed | InProgress, Cancel) => Some(Cancelled),
            (Done | Failed | Cancelled, Reopen) => Some(Open),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` rather than `write_str` so `{status:<11}` lines columns up.
        f.pad(self.as_str())
    }
}

impl FromStr for Status {
    type Err = UnknownStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Status::Open),
            "claimed" => Ok(Status::Claimed),
            "in_progress" | "in-progress" => Ok(Status::InProgress),
            "done" => Ok(Status::Done),
            "failed" => Ok(Status::Failed),
            "cancelled" | "canceled" => Ok(Status::Cancelled),
            other => Err(UnknownStatus(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "unknown status {0:?} (expected one of: open, claimed, in_progress, done, failed, cancelled)"
)]
pub struct UnknownStatus(pub String);

/// An edge in the status machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    /// Agent takes an `open` task.
    Claim,
    /// Agent reports it has started working.
    Start,
    /// Lease holder finishes successfully.
    Complete,
    /// Lease holder gives up.
    Fail,
    /// Lease ran out and the task returns to the pool.
    LeaseExpiry,
    /// Lease holder hands the task back unfinished.
    Release,
    /// Human abandons the task.
    Cancel,
    /// Human puts a terminal task back in the pool.
    Reopen,
}

impl Transition {
    /// Every transition, for exhaustive iteration in tests.
    pub const ALL: [Transition; 8] = [
        Transition::Claim,
        Transition::Start,
        Transition::Complete,
        Transition::Fail,
        Transition::LeaseExpiry,
        Transition::Release,
        Transition::Cancel,
        Transition::Reopen,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Transition::Claim => "claim",
            Transition::Start => "start",
            Transition::Complete => "complete",
            Transition::Fail => "fail",
            Transition::LeaseExpiry => "lease_expiry",
            Transition::Release => "release",
            Transition::Cancel => "cancel",
            Transition::Reopen => "reopen",
        }
    }
}

impl fmt::Display for Transition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// The kind of an entry in the append-only `task_events` trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Created,
    Claimed,
    Status,
    Note,
    LeaseRenewed,
    LeaseExpired,
    /// The holder handed the task back unfinished.
    Released,
    /// The holder released the task because it needs an answer from outside
    /// the agent queue before any agent can continue it.
    Asked,
    /// A human supplied the answer that makes an asking task claimable again.
    Answered,
    Completed,
    Failed,
    Cancelled,
    Reopened,
    /// A dependency on another task was added or removed.
    DepAdded,
    DepRemoved,
    /// The task's declared file scope changed.
    Scoped,
    /// The task was recused from another one's worker, or the bar was lifted.
    Recused,
    /// A review of this task's work was filed, or this task is that review.
    Reviewed,
    /// The witness saw the working tree change under this task.
    Witnessed,
    /// A finished task this one builds on stopped being finished — sent back
    /// by a review, reopened, cancelled or failed — while this task was held.
    GroundShifted,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Claimed => "claimed",
            EventKind::Status => "status",
            EventKind::Note => "note",
            EventKind::LeaseRenewed => "lease_renewed",
            EventKind::LeaseExpired => "lease_expired",
            EventKind::Released => "released",
            EventKind::Asked => "asked",
            EventKind::Answered => "answered",
            EventKind::Completed => "completed",
            EventKind::Failed => "failed",
            EventKind::Cancelled => "cancelled",
            EventKind::Reopened => "reopened",
            EventKind::DepAdded => "dep_added",
            EventKind::DepRemoved => "dep_removed",
            EventKind::Scoped => "scoped",
            EventKind::Recused => "recused",
            EventKind::Reviewed => "reviewed",
            EventKind::Witnessed => "witnessed",
            EventKind::GroundShifted => "ground_shifted",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "created" => Ok(EventKind::Created),
            "claimed" => Ok(EventKind::Claimed),
            "status" => Ok(EventKind::Status),
            "note" => Ok(EventKind::Note),
            "lease_renewed" => Ok(EventKind::LeaseRenewed),
            "lease_expired" => Ok(EventKind::LeaseExpired),
            "released" => Ok(EventKind::Released),
            "asked" => Ok(EventKind::Asked),
            "answered" => Ok(EventKind::Answered),
            "completed" => Ok(EventKind::Completed),
            "failed" => Ok(EventKind::Failed),
            "cancelled" => Ok(EventKind::Cancelled),
            "reopened" => Ok(EventKind::Reopened),
            "dep_added" => Ok(EventKind::DepAdded),
            "dep_removed" => Ok(EventKind::DepRemoved),
            "scoped" => Ok(EventKind::Scoped),
            "recused" => Ok(EventKind::Recused),
            "reviewed" => Ok(EventKind::Reviewed),
            "witnessed" => Ok(EventKind::Witnessed),
            "ground_shifted" => Ok(EventKind::GroundShifted),
            other => Err(format!("unknown event kind {other:?}")),
        }
    }
}

/// A unit of work, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub seq: i64,
    pub project: String,
    pub title: String,
    pub body: String,
    pub status: Status,
    pub priority: i64,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<String>,
    pub result: Option<String>,
    /// Whether finishing this task should put its work in front of another
    /// harness. See [`Recusal`] and DESIGN.md §15.
    pub review: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Task {
    /// Whole seconds left on the lease relative to `now`, if a lease is held.
    ///
    /// Negative once the lease is due for sweeping.
    pub fn lease_remaining_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        let expires = parse_ts(self.lease_expires_at.as_deref()?)?;
        Some((expires - now).num_seconds())
    }
}

/// The projection used by list views, which never need the task body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub seq: i64,
    pub project: String,
    pub title: String,
    pub status: Status,
    pub priority: i64,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<String>,
    pub updated_at: String,
}

/// One entry in a task's audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: String,
    pub task_id: String,
    pub at: String,
    pub actor: String,
    pub kind: EventKind,
    pub detail: String,
}

/// One question a holder asked before releasing a task.
///
/// An unanswered row is a derived readiness gate: the task remains `open`,
/// preserving the status machine, but no agent can claim it until the human
/// path fills the answer in. Earlier answered rows remain as handoff context
/// for every later claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub task_id: String,
    pub n: i64,
    pub asked_by: String,
    pub question: String,
    pub asked_at: String,
    pub answer: Option<String>,
    pub answered_by: Option<String>,
    pub answered_at: Option<String>,
}

impl Question {
    pub fn is_answered(&self) -> bool {
        self.answer.is_some()
    }
}

/// What it takes for a dependency to clear its dependents.
///
/// `done` was the whole answer until v1.7 made `done` revocable: a review can
/// send finished work back, and a dependent claimed in the meantime is building
/// on ground that may be pulled out from under it. Whether that possibility
/// holds the dependent back is policy, not fact, so it is configuration
/// (`under_review`) rather than code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Clearance {
    /// `done` clears a dependency, reviewed or not. The dependent is told the
    /// ground is provisional rather than kept waiting.
    #[default]
    Done,
    /// A `done` dependency with an unfinished review keeps holding its
    /// dependents until the review delivers its verdict — upheld releases
    /// them, sent back reopens the work and they wait on that instead.
    Reviewed,
}

/// A task that must finish before another one can start.
///
/// Carries enough context to explain a refusal without a second lookup, which
/// is what makes the error message actionable to a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub seq: i64,
    pub title: String,
    pub status: Status,
    /// The unfinished review of this blocker's work, where one exists. Only
    /// meaningful on a `done` blocker: it is what makes that `done`
    /// provisional rather than the last word.
    pub pending_review: Option<i64>,
}

impl Blocker {
    /// A blocker is cleared only by reaching `done`; a `failed` or `cancelled`
    /// dependency keeps the dependent task off the ready list, because the work
    /// it was waiting for did not happen. Under [`Clearance::Reviewed`], `done`
    /// under an unfinished review is not enough either — the verdict is still
    /// out, and a sent-back would take the ground away again.
    pub fn is_cleared(&self, clearance: Clearance) -> bool {
        self.status == Status::Done
            && (clearance == Clearance::Done || self.pending_review.is_none())
    }
}

/// One glob a task has declared it will touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathClaim {
    pub task_id: String,
    pub pattern: String,
    pub declared_by: String,
    pub at: String,
}

/// A task together with the file scope it has declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedTask {
    pub seq: i64,
    pub title: String,
    pub status: Status,
    pub holder: Option<String>,
    pub patterns: Vec<String>,
}

/// Two tasks whose declared file scopes can name the same path.
///
/// Reported whenever one of the two is being actively worked, which is the
/// only time an overlap can turn into a lost edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// The pattern declared by the task being checked.
    pub pattern: String,
    /// The task already in the way.
    pub other_seq: i64,
    pub other_title: String,
    pub other_pattern: String,
    pub other_status: Status,
    pub other_holder: Option<String>,
}

impl Conflict {
    /// One sentence, aimed at a model that has to relay it to a human.
    pub fn describe(&self) -> String {
        match &self.other_holder {
            Some(holder) => format!(
                "{} overlaps {} on task {} ({}), held by {holder}",
                self.pattern,
                self.other_pattern,
                self.other_seq,
                truncate_title(&self.other_title),
            ),
            None => format!(
                "{} overlaps {} on task {} ({}, {})",
                self.pattern,
                self.other_pattern,
                self.other_seq,
                truncate_title(&self.other_title),
                self.other_status,
            ),
        }
    }
}

fn truncate_title(title: &str) -> String {
    crate::fmt::truncate(title, 40)
}

/// One finished dependency, handed over at claim time: what the work it
/// produced says for itself, and how far that word can be trusted.
///
/// The dependency edge always was a context channel — "do the schema before
/// the API" means the API needs to know what the schema turned out to be — but
/// until v1.9 it was read only as a gate. The gate opens, the blocker's
/// `result` is dropped on the floor, and the dependent's agent starts blind
/// unless file overlap happens to carry something across. This is the row that
/// closes that gap: the claimant is handed each blocker's own summary without
/// knowing to ask for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ground {
    pub seq: i64,
    pub title: String,
    /// The summary its finisher wrote. `None` only for work finished outside
    /// the normal path — the column is what `task_complete` requires.
    pub result: Option<String>,
    pub standing: GroundStanding,
}

/// How much weight a finished dependency can bear.
///
/// Deliberately an echo of memory's `Standing` (§14): both answer "this was
/// true when it was written — is it still?", one for assertions, one for the
/// work a task builds on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundStanding {
    /// Finished, with nothing further on record.
    Done,
    /// Finished and seen to be finished: the newest verdict on it, from a
    /// harness that provably did not do the work, upheld it.
    Upheld,
    /// Finished, but its review has not delivered a verdict — a sent-back
    /// would reopen this work, so whatever builds on it is building on a
    /// provisional answer.
    UnderReview { review: i64 },
}

impl GroundStanding {
    /// `done`, `upheld`, `under review 15, provisional` — the words every
    /// front end uses for it.
    pub fn describe(self) -> String {
        match self {
            GroundStanding::Done => "done".to_string(),
            GroundStanding::Upheld => "upheld".to_string(),
            GroundStanding::UnderReview { review } => {
                format!("under review {review}, provisional")
            }
        }
    }

    pub fn is_provisional(self) -> bool {
        matches!(self, GroundStanding::UnderReview { .. })
    }
}

impl Ground {
    /// `#3 done`, `#3 upheld`, `#3 under review 15, provisional` — the label
    /// every front end uses.
    pub fn label(&self) -> String {
        format!("#{} {}", self.seq, self.standing.describe())
    }
}

/// A dependency that stopped being `done` while its dependent was being
/// worked.
///
/// Readiness is checked at the claim and never again, which was sound while
/// `done` was final. A verdict can now reopen finished work (§16), and a human
/// could always cancel or reopen it; either way, an agent mid-task is building
/// on ground that has moved, and it is the one participant with no way to
/// notice. This is what its next check-in tells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shifted {
    pub seq: i64,
    pub title: String,
    pub status: Status,
    /// The review whose sent-back reopened it, when that is what happened and
    /// nothing has happened since.
    pub sent_back_by: Option<i64>,
}

impl Shifted {
    /// One sentence, aimed at a model that has to decide what to do about it.
    pub fn describe(&self) -> String {
        let name = format!("task {} ({})", self.seq, truncate_title(&self.title));
        match (self.sent_back_by, self.status) {
            (Some(review), _) => format!(
                "{name}, which this task builds on, was sent back by review {review} and \
                 reopened; re-read it — the findings are in its brief — before building \
                 further on its work"
            ),
            (None, Status::Cancelled) => format!(
                "{name}, which this task builds on, has been cancelled since this task \
                 was claimed"
            ),
            (None, Status::Failed) => {
                format!("{name}, which this task builds on, has failed since this task was claimed")
            }
            (None, _) => format!(
                "{name}, which this task builds on, was reopened since this task was \
                 claimed and is no longer done"
            ),
        }
    }
}

/// One file the witness saw change while a task was held.
///
/// Deliberately not called "a file the task changed": a shared checkout cannot
/// tell you who typed. What it *can* tell you, exactly, is that the file is not
/// the file it was when the task started, and when that stopped being true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    /// Project-relative path.
    pub path: String,
    /// How it differs from the tree as it stood when the task was claimed:
    /// `added`, `modified` or `deleted`.
    pub kind: String,
    /// Content hash as of `last_seen`; empty when the file is gone.
    pub hash: String,
    pub first_seen: String,
    pub last_seen: String,
}

impl Observed {
    /// `src/config.rs (modified)`, the way every front end prints it.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.path, self.kind)
    }
}

/// One earlier holding of a task: who had it, how that ended, and what the
/// witness saw move while they did.
///
/// Archived at the moment a fresh claim would otherwise overwrite the
/// evidence, which is the only moment there is anything to archive. The
/// current holding is never in here — `task_witness` and `task_changes` are
/// its record — so `n` counts finished holdings, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenure {
    /// 1-based round number, per task.
    pub n: i64,
    /// Who held the lease. Empty when the baseline predates the column.
    pub holder: String,
    pub began_at: String,
    /// How the holding ended: `completed`, `failed`, `released`,
    /// `lease_expired` or `cancelled`. Empty when the trail could not say.
    pub ended: String,
    pub ended_at: String,
    /// What moved while it was held, frozen at the moment of archiving.
    pub changes: Vec<Observed>,
}

impl Tenure {
    /// `codex:9f2c` or, for a row old enough to predate the record, a phrase.
    fn who(&self) -> &str {
        if self.holder.is_empty() {
            "an earlier holder"
        } else {
            &self.holder
        }
    }

    /// How the holding ended, as the middle of a sentence.
    fn ending(&self) -> &'static str {
        match self.ended.as_str() {
            "completed" => "completed it",
            "failed" => "failed it",
            "released" => "handed it back unfinished",
            "lease_expired" => "went quiet until the lease expired",
            "cancelled" => "held it when it was cancelled",
            _ => "held it before this claim",
        }
    }

    /// The paths that moved, capped so a long list stays a sentence.
    fn moved(&self) -> String {
        const LISTED: usize = 6;
        let mut listed: Vec<String> = self
            .changes
            .iter()
            .take(LISTED)
            .map(Observed::describe)
            .collect();
        if self.changes.len() > LISTED {
            listed.push(format!("and {} more", self.changes.len() - LISTED));
        }
        listed.join(", ")
    }

    /// The sentence a successor is handed on claiming: what happened to this
    /// task before it was theirs, and where to read the details.
    ///
    /// The wording is careful about what the record can back. The changes are
    /// what moved *while the previous holding was live* — whatever state they
    /// were left in is part of the tree the new baseline was just read off,
    /// so the successor inherits it silently unless somebody says so. This is
    /// somebody saying so.
    pub fn describe(&self, seq: i64) -> String {
        if self.changes.is_empty() {
            return format!(
                "{} {} without the tree moving — there are no leftover edits to inherit",
                self.who(),
                self.ending()
            );
        }
        format!(
            "{} {}, and these files moved while they held it: {}. Whatever state they \
             left is part of the tree this claim starts from — `hird diff {seq} --tenure \
             {}` shows that round's changes before you build on or over them",
            self.who(),
            self.ending(),
            self.moved(),
            self.n
        )
    }

    /// The compact form `hird show` prints, one line per finished holding.
    pub fn label(&self) -> String {
        let ended = match self.ended.as_str() {
            "" => "ended".to_string(),
            kind => kind.replace('_', " "),
        };
        if self.changes.is_empty() {
            format!("round {}: {} — {ended}; read-only", self.n, self.who())
        } else {
            format!(
                "round {}: {} — {ended}; saw {}",
                self.n,
                self.who(),
                self.moved()
            )
        }
    }
}

/// Whether a task left a mark on the working tree, or only read it.
///
/// Every other witness type answers "what moved?", and answers it with a list
/// that comes back empty in two entirely different situations: a task that read
/// the code and wrote nothing, and a task hird was never watching. Those are
/// opposite claims — one is evidence, the other is the absence of any — and a
/// board that prints nothing in both cases invites a reader to take the second
/// for the first. This is the type that keeps them apart, and the only thing
/// that lets any front end say "read-only" out loud.
///
/// It is the same evidence [`Observed`] is, under the same limit: one checkout
/// has no keyboards, so `Modified` says the tree moved while the task was held
/// and not that this task's agent is who moved it. Where another task's
/// footprint holds one of the same files, `shared` says so rather than letting
/// a count speak with more confidence than it has earned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Footprint {
    /// No baseline was ever taken: the task has not been claimed under a
    /// witness, or the project is not a git checkout. hird has no opinion
    /// here, and "read-only" would be inventing one.
    #[default]
    Unwatched,
    /// A baseline was taken and the tree still matches it. Nothing moved while
    /// the task was held — or whatever moved was put back the way it was.
    ReadOnly,
    /// Files differ from the baseline the task was claimed against.
    Modified {
        /// How many paths differ.
        paths: usize,
        /// Whether another task in the project has one of them in its own
        /// footprint, which is to say it was live in that file at the same
        /// time and the change belongs to both records.
        shared: bool,
    },
}

impl Footprint {
    /// Whether the witness watched this task at all. `false` is the one case
    /// where nothing may be said either way.
    pub fn is_watched(self) -> bool {
        !matches!(self, Footprint::Unwatched)
    }

    /// A badge for a list — `read-only`, `modified 3 files` — or nothing at
    /// all where there is nothing to stand on.
    ///
    /// `live` is whether the task is still being worked, which is the
    /// difference between a verdict and a running total: a task that has not
    /// written anything yet has not finished not writing anything.
    pub fn label(self, live: bool) -> Option<String> {
        match self {
            Footprint::Unwatched => None,
            Footprint::ReadOnly if live => Some("read-only so far".to_string()),
            Footprint::ReadOnly => Some("read-only".to_string()),
            Footprint::Modified { paths, .. } => Some(format!(
                "modified {paths} file{}",
                if paths == 1 { "" } else { "s" }
            )),
        }
    }

    /// The compact form, for a board card whose whole width is a couple of
    /// dozen columns: `read-only`, `modified 3`.
    ///
    /// It drops the hedge [`Footprint::label`] carries for a task still being
    /// worked. Everything on a live board is a running total — the status, the
    /// lease, the count of open tasks — and a card with room for one badge
    /// should spend it on the fact rather than on saying so twice.
    pub fn badge(self) -> Option<String> {
        match self {
            Footprint::Unwatched => None,
            Footprint::ReadOnly => Some("read-only".to_string()),
            Footprint::Modified { paths, .. } => Some(format!("modified {paths}")),
        }
    }

    /// The same thing as a sentence, carrying the caveat where there is one.
    pub fn describe(self, live: bool) -> Option<String> {
        let label = self.label(live)?;
        Some(match self {
            Footprint::ReadOnly if live => {
                format!("{label} — nothing in the working tree has moved since it was claimed")
            }
            Footprint::ReadOnly => {
                format!("{label} — nothing in the working tree moved while it was held")
            }
            Footprint::Modified { shared: true, .. } => {
                format!("{label}, though another agent was live in some of them")
            }
            _ => label,
        })
    }
}

/// A task together with what the witness saw happen under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessedTask {
    pub seq: i64,
    pub title: String,
    pub status: Status,
    pub holder: Option<String>,
    pub changes: Vec<Observed>,
}

/// One file that moved while two tasks were both being worked.
///
/// This is the observed counterpart of [`Conflict`], and it is a stronger
/// statement: a conflict says two agents *might* end up in the same file, while
/// a contention says the file on disk changed during a window in which two
/// agents were both live in it. Whoever read it first is holding a copy that no
/// longer matches, and a write from that copy silently discards the other's
/// work — which is the failure the whole file-scope apparatus exists to catch,
/// finally visible rather than predicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contention {
    pub path: String,
    /// Content hash and time this task's side was last confirmed at.
    pub hash: String,
    pub last_seen: String,
    pub other_seq: i64,
    pub other_title: String,
    pub other_status: Status,
    pub other_holder: Option<String>,
    pub other_hash: String,
    pub other_last_seen: String,
}

impl Contention {
    /// Do the two sides disagree about what the file says?
    ///
    /// Each records the version it was last shown, so disagreement means one of
    /// them is behind. Agreement means nobody is, which is why a contention
    /// where this is false is never reported: two agents up to date on the same
    /// file have nothing to act on, and a warning nobody needs to act on is a
    /// warning that gets ignored the once it matters. What is left is the
    /// declared overlap, which the board already shows.
    pub fn is_stale(&self) -> bool {
        self.hash != self.other_hash
    }

    /// One sentence, aimed at a model that has to relay it to a human.
    ///
    /// It says what is certainly true and what to do about it, and stops there:
    /// hird watched a file change, it did not watch anybody type.
    pub fn describe(&self) -> String {
        let who = match &self.other_holder {
            Some(holder) => format!(
                "task {} ({}), held by {holder}",
                self.other_seq,
                truncate_title(&self.other_title)
            ),
            None => format!(
                "task {} ({}, {})",
                self.other_seq,
                truncate_title(&self.other_title),
                self.other_status
            ),
        };
        // Only a strictly later confirmation on this side lets us say the
        // other agent is the one holding the old copy. Anything else — theirs
        // later, or the two too close together to order — is answered with the
        // warning, because being told to re-read a file you already have is
        // cheap and losing an edit is not.
        if self.last_seen > self.other_last_seen {
            format!(
                "{} changed under {who}, and again on your side afterwards — check you did \
                 not write over their edit",
                self.path
            )
        } else {
            format!(
                "{} changed under {who} at {}, at or after the version hird last confirmed \
                 for you — re-read it before you write, or your edit will discard theirs",
                self.path,
                short_time(&self.other_last_seen),
            )
        }
    }
}

/// `2026-07-25T14:32:07.001Z` → `14:32`, for a sentence rather than a log line.
fn short_time(ts: &str) -> String {
    match parse_ts(ts) {
        Some(dt) => dt.format("%H:%M UTC").to_string(),
        None => ts.to_string(),
    }
}

/// A task whose worker is barred from working another one.
///
/// Carries who that is, so a refusal can name them without a second lookup —
/// the same reason [`Blocker`] carries a title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recusal {
    pub from_seq: i64,
    pub from_title: String,
    /// Why the bar exists, in the words of whoever filed it.
    pub reason: String,
    /// Whoever last held the task being recused from, if anybody has. `None`
    /// means the work has not been done yet, so the recusal bars nobody — a
    /// constraint waiting for something to constrain.
    pub worker: Option<String>,
}

impl Recusal {
    /// One sentence, aimed at a model that has to relay it to a human.
    pub fn describe(&self) -> String {
        let what = format!(
            "task {} ({})",
            self.from_seq,
            crate::fmt::truncate(&self.from_title, 40)
        );
        let why = if self.reason.is_empty() {
            String::new()
        } else {
            format!(" — {}", self.reason)
        };
        match &self.worker {
            Some(worker) => format!(
                "not whoever worked {what}: that was {worker}, so this needs another harness{why}"
            ),
            None => format!("not whoever works {what}; nobody has yet{why}"),
        }
    }
}

/// What a review concluded about the work it reviewed.
///
/// A review's `result` is prose; the verdict is the one bit of it the queue
/// can act on. `Upheld` means the work stands — done, and seen to be done by a
/// harness that did not do it. `SentBack` means it does not, and names its own
/// consequence: the work returns to the pool carrying the reviewer's findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Upheld,
    SentBack,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Upheld => "upheld",
            Verdict::SentBack => "sent_back",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for Verdict {
    type Err = UnknownVerdict;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "upheld" | "uphold" | "approve" | "approved" => Ok(Verdict::Upheld),
            "sent_back" | "sent-back" | "send_back" | "send-back" | "needs_work" | "needs-work" => {
                Ok(Verdict::SentBack)
            }
            other => Err(UnknownVerdict(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "unknown verdict {0:?}: say \"upheld\" if the work stands, or \"sent_back\" to return it \
     to the pool carrying your findings"
)]
pub struct UnknownVerdict(pub String);

/// One verdict, as delivered: which review said it, about whose work.
///
/// Append-only, like the event trail — a work task sent back and redone
/// accumulates one of these per round, which is what makes the record honest
/// about how many rounds there were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictRecord {
    pub review_seq: i64,
    pub task_seq: i64,
    pub verdict: Verdict,
    /// Whoever's work was judged: the holder of record when the verdict
    /// landed. Empty when nobody had worked the task.
    pub worker: String,
    /// Whoever delivered it.
    pub reviewer: String,
    pub at: String,
}

impl VerdictRecord {
    /// One sentence, the way the board and `task_get` say it.
    pub fn describe(&self) -> String {
        match self.verdict {
            Verdict::Upheld => format!(
                "upheld by {} (review {})",
                crate::identity::actor_harness(&self.reviewer),
                self.review_seq
            ),
            Verdict::SentBack => format!(
                "sent back by {} (review {})",
                crate::identity::actor_harness(&self.reviewer),
                self.review_seq
            ),
        }
    }
}

/// One harness's standing in the verdict record, both as worker and reviewer.
///
/// Derived entirely from delivered verdicts, so it measures the one thing the
/// queue can measure: whose work survives a reading by a different model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessRecord {
    pub harness: String,
    /// Verdicts received on this harness's work, across every round.
    pub judged: i64,
    pub upheld: i64,
    pub sent_back: i64,
    /// Distinct tasks judged, and how many of them were upheld on the first
    /// verdict — before any round of rework.
    pub tasks_judged: i64,
    pub first_pass: i64,
    /// Verdicts this harness has delivered as a reviewer.
    pub upheld_given: i64,
    pub sent_back_given: i64,
}

/// One file an assertion was learned against, and what that file said then.
///
/// An assertion is a statement *about code*, and code moves. The anchor is the
/// receipt: this is the file the claim was read off, and this is the version of
/// it that was open at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Project-relative path.
    pub path: String,
    /// Content hash when the anchor was taken; empty if the file was already
    /// absent, which is a fact worth keeping as much as any other.
    pub hash: String,
    pub at: String,
}

/// A file an assertion stands on that is no longer what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shift {
    pub path: String,
    /// The file is not merely different, it is not there.
    pub gone: bool,
}

/// How much the ground under an assertion has moved since it was recorded.
///
/// This is the one question every agent-memory store fails to answer and every
/// one of them needs to: a fact recorded six weeks ago about a file that has
/// been rewritten twice since is not a fact any more, and nothing about the
/// sentence itself gives that away. hird can answer it because the witness
/// already fingerprints files, so an assertion can be stored alongside the
/// version of the code it was read off.
///
/// Note what it deliberately is not: a verdict. A file changing does not make
/// an assertion false — a rename, a formatting pass and a rewrite all look
/// identical from here. It makes it *unverified*, which is a different and much
/// more useful thing to be told, because it is exactly the set of facts worth
/// spending a re-read on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "standing")]
pub enum Standing {
    /// No files were ever recorded, so there is nothing to check it against.
    Unanchored,
    /// Every file it was recorded against still says what it said.
    Firm { paths: Vec<String> },
    /// At least one of them does not.
    Shaky {
        moved: Vec<Shift>,
        firm: Vec<String>,
    },
    /// All of them are gone.
    Orphaned { paths: Vec<String> },
}

impl Standing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Standing::Unanchored => "unanchored",
            Standing::Firm { .. } => "firm",
            Standing::Shaky { .. } => "shaky",
            Standing::Orphaned { .. } => "orphaned",
        }
    }

    /// Every file this standing was decided from, in order.
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Standing::Unanchored => Vec::new(),
            Standing::Firm { paths } | Standing::Orphaned { paths } => {
                paths.iter().map(String::as_str).collect()
            }
            Standing::Shaky { moved, firm } => moved
                .iter()
                .map(|s| s.path.as_str())
                .chain(firm.iter().map(String::as_str))
                .collect(),
        }
    }

    /// Is this worth re-reading the code over?
    pub fn needs_checking(&self) -> bool {
        matches!(self, Standing::Shaky { .. } | Standing::Orphaned { .. })
    }

    /// How much to trust it, lowest first. Used to order recall, where the
    /// budget is small and a verified fact should outrank a suspect one.
    pub fn rank(&self) -> u8 {
        match self {
            Standing::Firm { .. } => 0,
            Standing::Unanchored => 1,
            Standing::Shaky { .. } => 2,
            Standing::Orphaned { .. } => 3,
        }
    }

    /// One sentence, aimed at a model deciding whether to trust the assertion.
    ///
    /// `None` for an unanchored assertion: hird has nothing to say about it,
    /// and saying so on every row would be noise wearing the clothes of a
    /// warning.
    pub fn describe(&self) -> Option<String> {
        match self {
            Standing::Unanchored => None,
            Standing::Firm { paths } => Some(match paths.as_slice() {
                [one] => format!("{one} is unchanged since this was recorded"),
                many => format!(
                    "all {} files this was recorded against are unchanged",
                    many.len()
                ),
            }),
            Standing::Shaky { moved, .. } => Some(format!(
                "{} since this was recorded — re-read before relying on it",
                describe_shifts(moved)
            )),
            Standing::Orphaned { paths } => Some(match paths.as_slice() {
                [one] => {
                    format!("{one} no longer exists — this was recorded about a file that has gone")
                }
                many => format!(
                    "all {} files this was recorded against have been deleted",
                    many.len()
                ),
            }),
        }
    }
}

/// `src/config.rs has changed`, `src/a.rs is gone and 2 others have changed`.
fn describe_shifts(moved: &[Shift]) -> String {
    let verb = |s: &Shift| if s.gone { "is gone" } else { "has changed" };
    match moved {
        [] => "nothing has changed".to_string(),
        [one] => format!("{} {}", one.path, verb(one)),
        [first, rest @ ..] => {
            let others = rest.len();
            let tail = match (others == 1, rest.iter().all(|s| s.gone)) {
                (true, true) => "is gone",
                (true, false) => "has changed",
                (false, true) => "are gone",
                (false, false) => "have changed",
            };
            format!(
                "{} {} and {others} other{} {tail}",
                first.path,
                verb(first),
                if others == 1 { "" } else { "s" },
            )
        }
    }
}

/// Everyone who has stated an assertion, counting whoever recorded it first.
///
/// Two agents in one harness saying the same thing is repetition; two agents in
/// *different* harnesses saying it independently is corroboration, and hird is
/// the only thing in the room that can tell the difference, because it is the
/// only thing both of them talk to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voices {
    /// Actors, the agent that first recorded the assertion first.
    pub actors: Vec<String>,
}

impl Voices {
    /// The distinct harnesses among them, in first-seen order.
    pub fn harnesses(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for actor in &self.actors {
            let harness = crate::identity::actor_harness(actor);
            if !out.contains(&harness) {
                out.push(harness);
            }
        }
        out
    }

    /// One sentence, or `None` when only one voice has ever said it — which is
    /// the ordinary case and not worth a line.
    ///
    /// Names them rather than counting them, because who confirmed a fact is
    /// most of what the count was standing in for, and a reader can weigh
    /// `codex:9f2c` against `cli` for themselves. Long lists are cut off with a
    /// tally rather than truncated silently.
    pub fn describe(&self) -> Option<String> {
        let [_first, others @ ..] = self.actors.as_slice() else {
            return None;
        };
        if others.is_empty() {
            return None;
        }
        const NAMED: usize = 3;
        let shown: Vec<&str> = others.iter().take(NAMED).map(String::as_str).collect();
        let mut sentence = format!("also stated by {}", shown.join(", "));
        if others.len() > shown.len() {
            sentence.push_str(&format!(" and {} more", others.len() - shown.len()));
        }
        // The claim worth making loudest: not that several sessions agree, but
        // that sessions which cannot see each other agree.
        let harnesses = self.harnesses().len();
        if harnesses > 1 {
            sentence.push_str(&format!(", independently across {harnesses} harnesses"));
        }
        Some(sentence)
    }
}

/// One durable factual claim recorded by an agent or a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    pub id: String,
    pub project: String,
    pub content: String,
    pub tags: String,
    pub actor: String,
    pub task_id: Option<String>,
    /// Set to the id of the assertion that replaced this one; `None` = current.
    pub superseded_by: Option<String>,
    pub created_at: String,
}

impl Assertion {
    /// Tags split into trimmed, non-empty pieces.
    pub fn tag_list(&self) -> Vec<&str> {
        self.tags
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// Normalize freeform tag input into the comma-separated storage form.
pub fn normalize_tags(raw: &str) -> String {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reachable set from `open` must be exactly the six statuses, and no
    /// transition may produce a status outside the diagram in §5 of DESIGN.md.
    #[test]
    fn status_machine_has_no_edges_outside_the_diagram() {
        // The complete edge list, transcribed independently from the diagram.
        let allowed: &[(Status, Transition, Status)] = &[
            (Status::Open, Transition::Claim, Status::Claimed),
            (Status::Claimed, Transition::Start, Status::InProgress),
            (Status::InProgress, Transition::Start, Status::InProgress),
            (Status::Claimed, Transition::Complete, Status::Done),
            (Status::InProgress, Transition::Complete, Status::Done),
            (Status::Claimed, Transition::Fail, Status::Failed),
            (Status::InProgress, Transition::Fail, Status::Failed),
            (Status::Claimed, Transition::LeaseExpiry, Status::Open),
            (Status::InProgress, Transition::LeaseExpiry, Status::Open),
            (Status::Claimed, Transition::Release, Status::Open),
            (Status::InProgress, Transition::Release, Status::Open),
            (Status::Open, Transition::Cancel, Status::Cancelled),
            (Status::Claimed, Transition::Cancel, Status::Cancelled),
            (Status::InProgress, Transition::Cancel, Status::Cancelled),
            (Status::Done, Transition::Reopen, Status::Open),
            (Status::Failed, Transition::Reopen, Status::Open),
            (Status::Cancelled, Transition::Reopen, Status::Open),
        ];

        for from in Status::ALL {
            for transition in Transition::ALL {
                let expected = allowed
                    .iter()
                    .find(|(f, t, _)| *f == from && *t == transition)
                    .map(|(_, _, to)| *to);
                assert_eq!(
                    from.apply(transition),
                    expected,
                    "edge {from} --{transition}--> mismatch"
                );
            }
        }
    }

    #[test]
    fn every_status_is_reachable_from_open() {
        let mut seen = std::collections::BTreeSet::from([Status::Open]);
        loop {
            let next: Vec<Status> = seen
                .iter()
                .flat_map(|s| Transition::ALL.iter().filter_map(|t| s.apply(*t)))
                .collect();
            let before = seen.len();
            seen.extend(next);
            if seen.len() == before {
                break;
            }
        }
        assert_eq!(seen, Status::ALL.into_iter().collect());
    }

    #[test]
    fn terminal_statuses_only_leave_via_reopen() {
        for status in Status::ALL.into_iter().filter(|s| s.is_terminal()) {
            for transition in Transition::ALL {
                let reached = status.apply(transition);
                if transition == Transition::Reopen {
                    assert_eq!(reached, Some(Status::Open));
                } else {
                    assert_eq!(reached, None, "{status} escaped via {transition}");
                }
            }
        }
    }

    #[test]
    fn status_round_trips_through_its_string_form() {
        for status in Status::ALL {
            assert_eq!(status.as_str().parse::<Status>().unwrap(), status);
        }
    }

    #[test]
    fn timestamps_sort_lexicographically() {
        let early = fmt_ts(DateTime::from_timestamp(1_700_000_000, 0).unwrap());
        let late = fmt_ts(DateTime::from_timestamp(1_800_000_000, 0).unwrap());
        assert!(early < late);
        assert_eq!(early.len(), late.len());
        assert_eq!(parse_ts(&early).unwrap().timestamp(), 1_700_000_000);
    }

    /// The sentence a shaky assertion carries is the whole product: it has to
    /// name the file, say what happened to it, and say what to do — without
    /// ever claiming the assertion is false, which hird cannot know.
    #[test]
    fn a_shaky_standing_names_the_file_and_stops_short_of_a_verdict() {
        let one = Standing::Shaky {
            moved: vec![Shift {
                path: "src/config.rs".into(),
                gone: false,
            }],
            firm: vec![],
        };
        assert_eq!(
            one.describe().unwrap(),
            "src/config.rs has changed since this was recorded — re-read before relying on it"
        );
        for word in ["wrong", "false", "stale", "invalid"] {
            assert!(!one.describe().unwrap().contains(word), "{word}");
        }

        let several = Standing::Shaky {
            moved: vec![
                Shift {
                    path: "src/a.rs".into(),
                    gone: true,
                },
                Shift {
                    path: "src/b.rs".into(),
                    gone: false,
                },
            ],
            firm: vec!["src/c.rs".into()],
        };
        assert!(
            several
                .describe()
                .unwrap()
                .starts_with("src/a.rs is gone and 1 other has changed"),
            "{:?}",
            several.describe()
        );
    }

    #[test]
    fn standings_report_every_path_they_were_decided_from() {
        let shaky = Standing::Shaky {
            moved: vec![Shift {
                path: "a".into(),
                gone: false,
            }],
            firm: vec!["b".into()],
        };
        assert_eq!(shaky.paths(), vec!["a", "b"]);
        assert!(Standing::Unanchored.paths().is_empty());
        assert_eq!(
            Standing::Firm {
                paths: vec!["a".into()]
            }
            .paths(),
            vec!["a"]
        );
    }

    /// One agent saying something is provenance. Two agents in two harnesses
    /// saying it independently is the strongest signal hird can produce, and it
    /// is the one nothing else in the room is positioned to see.
    #[test]
    fn corroboration_is_only_worth_a_sentence_when_it_crosses_agents() {
        let alone = Voices {
            actors: vec!["codex:9f2c".into()],
        };
        assert!(alone.describe().is_none());

        let same_harness = Voices {
            actors: vec!["codex:9f2c".into(), "codex:1a2b".into()],
        };
        assert_eq!(
            same_harness.describe().unwrap(),
            "also stated by codex:1a2b",
            "two sessions of one harness are not independent confirmation"
        );
        assert_eq!(same_harness.harnesses(), vec!["codex"]);

        let across = Voices {
            actors: vec![
                "codex:9f2c".into(),
                "claude-code:af31".into(),
                "copilot:77".into(),
            ],
        };
        assert_eq!(
            across.describe().unwrap(),
            "also stated by claude-code:af31, copilot:77, independently across 3 harnesses"
        );

        // A long list is cut off with a tally, never silently truncated.
        let many = Voices {
            actors: (0..6).map(|i| format!("codex:{i}")).collect(),
        };
        let sentence = many.describe().unwrap();
        assert!(sentence.contains("and 2 more"), "{sentence}");
    }

    #[test]
    fn tags_are_normalized_to_a_compact_comma_list() {
        assert_eq!(normalize_tags(" a , ,b,, c "), "a,b,c");
        assert_eq!(normalize_tags(""), "");
    }

    /// The distinction the type exists for: an unwatched task says nothing,
    /// and it must not be possible to mistake that for "nothing moved".
    #[test]
    fn an_unwatched_task_makes_no_claim_either_way() {
        assert_eq!(Footprint::Unwatched.label(false), None);
        assert_eq!(Footprint::Unwatched.describe(false), None);
        assert_eq!(Footprint::default(), Footprint::Unwatched);
        assert!(!Footprint::Unwatched.is_watched());
        assert!(Footprint::ReadOnly.is_watched());
    }

    /// A task still being worked has not finished not writing anything, and
    /// the wording has to leave room for its next edit.
    #[test]
    fn read_only_is_a_verdict_when_finished_and_a_running_total_while_held() {
        assert_eq!(Footprint::ReadOnly.label(false).unwrap(), "read-only");
        assert_eq!(Footprint::ReadOnly.label(true).unwrap(), "read-only so far");
        assert!(Footprint::ReadOnly
            .describe(false)
            .unwrap()
            .contains("while it was held"));
        assert!(Footprint::ReadOnly
            .describe(true)
            .unwrap()
            .contains("since it was claimed"));
    }

    #[test]
    fn a_shared_file_stops_a_count_from_speaking_for_one_agent() {
        let alone = Footprint::Modified {
            paths: 1,
            shared: false,
        };
        assert_eq!(alone.label(false).unwrap(), "modified 1 file");
        assert_eq!(alone.describe(false).unwrap(), "modified 1 file");

        let shared = Footprint::Modified {
            paths: 3,
            shared: true,
        };
        assert_eq!(shared.label(true).unwrap(), "modified 3 files");
        let sentence = shared.describe(true).unwrap();
        assert!(sentence.starts_with("modified 3 files,"), "{sentence}");
        assert!(sentence.contains("another agent"), "{sentence}");
    }
}
