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
    ///   └──────── reopen (human) ◄── done|failed|cancelled
    /// open ──cancel (human)──► cancelled
    /// ```
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
            (Claimed | InProgress, LeaseExpiry) => Some(Open),
            (Open | Claimed | InProgress, Cancel) => Some(Cancelled),
            (Done | Failed | Cancelled, Reopen) => Some(Open),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    /// Human abandons the task.
    Cancel,
    /// Human puts a terminal task back in the pool.
    Reopen,
}

impl Transition {
    /// Every transition, for exhaustive iteration in tests.
    pub const ALL: [Transition; 7] = [
        Transition::Claim,
        Transition::Start,
        Transition::Complete,
        Transition::Fail,
        Transition::LeaseExpiry,
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
            Transition::Cancel => "cancel",
            Transition::Reopen => "reopen",
        }
    }
}

impl fmt::Display for Transition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    Completed,
    Failed,
    Cancelled,
    Reopened,
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
            EventKind::Completed => "completed",
            EventKind::Failed => "failed",
            EventKind::Cancelled => "cancelled",
            EventKind::Reopened => "reopened",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
            "completed" => Ok(EventKind::Completed),
            "failed" => Ok(EventKind::Failed),
            "cancelled" => Ok(EventKind::Cancelled),
            "reopened" => Ok(EventKind::Reopened),
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

    #[test]
    fn tags_are_normalized_to_a_compact_comma_list() {
        assert_eq!(normalize_tags(" a , ,b,, c "), "a,b,c");
        assert_eq!(normalize_tags(""), "");
    }
}
