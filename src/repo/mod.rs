//! Typed repository layer. All SQL in `hird` lives here.
//!
//! The MCP, CLI and TUI layers call these methods and never write SQL of their
//! own — see the quality bar in DESIGN.md §11.

mod deps;
mod events;
pub(crate) mod footing;
mod memory;
mod plan;
mod questions;
mod recall;
mod recess;
mod recusal;
mod requirements;
mod scope;
mod tasks;
mod verdict;
mod witness;

pub use deps::{dispatch_waves, Claimable, Deps};
pub use events::{Events, FeedEvent, FeedFilter, ReplayedTask};
pub use footing::Footings;
pub use memory::{Memory, MemoryQuery, NewAssertion, Recorded};
pub use plan::{Applied, Drift, Placed, Plans};
pub use questions::Questions;
pub use recall::{Recall, RecallReason, Recalled};
pub use recess::{Called, Recesses};
pub use recusal::Recusals;
pub use requirements::Requirements;
pub use scope::{OnConflict, Scopes};
pub use tasks::{Claim, Dispatch, Finished, Subtask, SweepOutcome, Tasks};
pub use verdict::{Delivered, Verdicts};
pub use witness::{Baseline, Witnessed};

/// Pattern validation, shared with the plan format so a plan file is refused
/// for exactly the reasons a declaration would be.
pub(crate) use scope::normalize_all;

use ulid::Ulid;

/// Fresh primary key. ULIDs sort by creation time, which keeps
/// `ORDER BY id` meaningful for the append-only tables.
pub(crate) fn new_id() -> String {
    Ulid::generate().to_string()
}

/// The transaction every writer in this layer uses.
///
/// IMMEDIATE takes the write lock up front, so two concurrent writers queue on
/// `busy_timeout` instead of deadlocking on a deferred read-to-write upgrade
/// (which SQLite fails without retrying). One function so that posture is
/// stated once and no repository can quietly open a weaker one.
pub(crate) fn immediate_tx(
    conn: &rusqlite::Connection,
) -> crate::error::Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    )?)
}

/// How list queries scope to a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectScope {
    /// Only rows belonging to this project root.
    Only(String),
    /// Every project in the database.
    All,
}

impl ProjectScope {
    /// Build a scope from the "current project + `all_projects` flag" pair the
    /// MCP tools and CLI both use.
    pub fn resolve(project: &str, all_projects: bool) -> ProjectScope {
        if all_projects {
            ProjectScope::All
        } else {
            ProjectScope::Only(project.to_string())
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, ProjectScope::All)
    }

    /// The SQL fragment and bound value for a `WHERE` clause.
    fn clause(&self, column: &str) -> (String, Option<&str>) {
        match self {
            ProjectScope::All => ("1 = 1".to_string(), None),
            ProjectScope::Only(p) => (format!("{column} = ?"), Some(p.as_str())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_resolves_from_the_all_projects_flag() {
        assert_eq!(
            ProjectScope::resolve("/tmp/p", false),
            ProjectScope::Only("/tmp/p".into())
        );
        assert_eq!(ProjectScope::resolve("/tmp/p", true), ProjectScope::All);
    }

    #[test]
    fn ids_are_unique_and_time_ordered() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), b.len());
    }
}
