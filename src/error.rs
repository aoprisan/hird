//! Error type shared by the repository layer.
//!
//! Every variant's `Display` is written to be relayed verbatim to a human or a
//! model — see the "errors are descriptive strings" rule in DESIGN.md §6.

use crate::model::{Blocker, Conflict, Recusal, Status};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("task {seq} not found")]
    TaskNotFound { seq: i64 },

    #[error("assertion {id} not found")]
    AssertionNotFound { id: String },

    /// A claim lost the race, or the task was never claimable.
    #[error("{}", claim_conflict_message(*.seq, *.status, .holder.as_deref(), .lease_expires_at.as_deref()))]
    ClaimConflict {
        seq: i64,
        status: Status,
        holder: Option<String>,
        lease_expires_at: Option<String>,
    },

    /// Someone other than the lease holder tried to drive the task.
    #[error("{}", not_holder_message(*.seq, *.status, .holder.as_deref(), .actor))]
    NotHolder {
        seq: i64,
        status: Status,
        holder: Option<String>,
        actor: String,
    },

    /// The requested move is not an edge of the status machine.
    #[error("cannot {transition} task {seq}: it is {status}")]
    InvalidTransition {
        seq: i64,
        status: Status,
        transition: &'static str,
    },

    /// The task's dependencies have not all finished.
    #[error("{}", blocked_message(*.seq, .blockers))]
    Blocked { seq: i64, blockers: Vec<Blocker> },

    /// Adding this dependency would close a cycle in the graph.
    #[error("{}", cycle_message(*.seq, *.on, .path))]
    DependencyCycle {
        seq: i64,
        on: i64,
        /// The existing chain from `on` back to `seq`, which the new edge
        /// would close.
        path: Vec<i64>,
    },

    /// This harness is the one barred from working the task.
    #[error("{}", recused_message(*.seq, .recusal, .actor))]
    Recused {
        seq: i64,
        recusal: Recusal,
        actor: String,
    },

    /// The declared file scope overlaps work another agent is doing.
    #[error("{}", conflict_message(*.seq, .conflicts))]
    PathConflict { seq: i64, conflicts: Vec<Conflict> },

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
}

fn claim_conflict_message(
    seq: i64,
    status: Status,
    holder: Option<&str>,
    lease_expires_at: Option<&str>,
) -> String {
    match (holder, lease_expires_at) {
        (Some(holder), Some(expires)) => {
            format!("task {seq} is {status} by {holder} until {expires}")
        }
        (Some(holder), None) => format!("task {seq} is {status} by {holder}"),
        _ => format!("task {seq} is {status}, not open, so it cannot be claimed"),
    }
}

fn blocked_message(seq: i64, blockers: &[Blocker]) -> String {
    let listed = join_and(blockers.iter().map(|b| {
        format!(
            "task {} ({}, {})",
            b.seq,
            crate::fmt::truncate(&b.title, 40),
            b.status
        )
    }));
    format!(
        "task {seq} is blocked by {listed}; it becomes claimable once every \
         dependency is done"
    )
}

fn cycle_message(seq: i64, on: i64, path: &[i64]) -> String {
    let chain = path
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" -> ");
    format!(
        "task {seq} cannot depend on task {on}: task {on} already depends on \
         task {seq} ({chain}), and that would be a cycle"
    )
}

fn recused_message(seq: i64, recusal: &Recusal, actor: &str) -> String {
    let worker = recusal.worker.as_deref().unwrap_or("nobody");
    let reason = if recusal.reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", recusal.reason)
    };
    format!(
        "task {seq} is recused from task {} ({}){reason}, which {worker} worked — so {actor} \
         cannot take it. Tell the human it needs an agent in a different harness.",
        recusal.from_seq,
        crate::fmt::truncate(&recusal.from_title, 40),
    )
}

fn conflict_message(seq: i64, conflicts: &[Conflict]) -> String {
    let listed = join_and(conflicts.iter().map(Conflict::describe));
    format!("task {seq} would collide with work already under way: {listed}")
}

/// `a`, `a and b`, `a, b and c` — used wherever an error names several things.
fn join_and(items: impl IntoIterator<Item = String>) -> String {
    let items: Vec<String> = items.into_iter().collect();
    match items.split_last() {
        None => "nothing".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

fn not_holder_message(seq: i64, status: Status, holder: Option<&str>, actor: &str) -> String {
    match holder {
        Some(holder) => format!(
            "task {seq} is held by {holder}, not {actor}; only the lease holder can update it"
        ),
        None => format!("task {seq} is {status} and unclaimed; {actor} must claim it first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_conflict_reads_like_the_design_example() {
        let err = Error::ClaimConflict {
            seq: 42,
            status: Status::Claimed,
            holder: Some("codex:9f2c".into()),
            lease_expires_at: Some("2026-07-25T14:32:00.000Z".into()),
        };
        assert_eq!(
            err.to_string(),
            "task 42 is claimed by codex:9f2c until 2026-07-25T14:32:00.000Z"
        );
    }

    #[test]
    fn claim_conflict_without_a_holder_explains_the_status() {
        let err = Error::ClaimConflict {
            seq: 7,
            status: Status::Done,
            holder: None,
            lease_expires_at: None,
        };
        assert_eq!(
            err.to_string(),
            "task 7 is done, not open, so it cannot be claimed"
        );
    }

    #[test]
    fn not_holder_names_both_parties() {
        let err = Error::NotHolder {
            seq: 3,
            status: Status::InProgress,
            holder: Some("codex:9f2c".into()),
            actor: "claude-code:af31".into(),
        };
        assert_eq!(
            err.to_string(),
            "task 3 is held by codex:9f2c, not claude-code:af31; \
             only the lease holder can update it"
        );
    }
}
