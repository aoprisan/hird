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

    /// Terminal statuses can only leave via a human `reopen`.
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
    ///   └──────── reopen (human) ◄── done|failed|cancelled
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
    Completed,
    Failed,
    Cancelled,
    Reopened,
    /// A dependency on another task was added or removed.
    DepAdded,
    DepRemoved,
    /// The task's declared file scope changed.
    Scoped,
    /// The witness saw the working tree change under this task.
    Witnessed,
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
            EventKind::Completed => "completed",
            EventKind::Failed => "failed",
            EventKind::Cancelled => "cancelled",
            EventKind::Reopened => "reopened",
            EventKind::DepAdded => "dep_added",
            EventKind::DepRemoved => "dep_removed",
            EventKind::Scoped => "scoped",
            EventKind::Witnessed => "witnessed",
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
            "completed" => Ok(EventKind::Completed),
            "failed" => Ok(EventKind::Failed),
            "cancelled" => Ok(EventKind::Cancelled),
            "reopened" => Ok(EventKind::Reopened),
            "dep_added" => Ok(EventKind::DepAdded),
            "dep_removed" => Ok(EventKind::DepRemoved),
            "scoped" => Ok(EventKind::Scoped),
            "witnessed" => Ok(EventKind::Witnessed),
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

/// A task that must finish before another one can start.
///
/// Carries enough context to explain a refusal without a second lookup, which
/// is what makes the error message actionable to a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub seq: i64,
    pub title: String,
    pub status: Status,
}

impl Blocker {
    /// A blocker is cleared only by reaching `done`; a `failed` or `cancelled`
    /// dependency keeps the dependent task off the ready list, because the work
    /// it was waiting for did not happen.
    pub fn is_cleared(&self) -> bool {
        self.status == Status::Done
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

    /// One sentence, or `None` when only one agent has ever said it — which is
    /// the ordinary case and not worth a line.
    pub fn describe(&self) -> Option<String> {
        if self.actors.len() < 2 {
            return None;
        }
        let harnesses = self.harnesses();
        let others = self.actors.len() - 1;
        let plural = if others == 1 { "" } else { "s" };
        if harnesses.len() > 1 {
            Some(format!(
                "confirmed since by {others} other agent{plural}, across {} harnesses ({})",
                harnesses.len(),
                harnesses.join(", "),
            ))
        } else {
            Some(format!("confirmed since by {others} other agent{plural}"))
        }
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
        let sentence = same_harness.describe().unwrap();
        assert_eq!(sentence, "confirmed since by 1 other agent");
        assert_eq!(same_harness.harnesses(), vec!["codex"]);

        let across = Voices {
            actors: vec![
                "codex:9f2c".into(),
                "claude-code:af31".into(),
                "copilot:77".into(),
            ],
        };
        let sentence = across.describe().unwrap();
        assert!(sentence.contains("2 other agents"), "{sentence}");
        assert!(sentence.contains("3 harnesses"), "{sentence}");
        assert!(
            sentence.contains("codex, claude-code, copilot"),
            "{sentence}"
        );
    }

    #[test]
    fn tags_are_normalized_to_a_compact_comma_list() {
        assert_eq!(normalize_tags(" a , ,b,, c "), "a,b,c");
        assert_eq!(normalize_tags(""), "");
    }
}
