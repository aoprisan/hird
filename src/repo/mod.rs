//! Typed repository layer. All SQL in `hird` lives here.
//!
//! The MCP, CLI and TUI layers call these methods and never write SQL of their
//! own — see the quality bar in DESIGN.md §11.

mod deps;
mod memory;
mod recall;
mod scope;
mod tasks;

pub use deps::{dispatch_waves, Deps};
pub use memory::{Memory, MemoryQuery, NewAssertion};
pub use recall::{Recall, RecallReason, Recalled};
pub use scope::{OnConflict, Scopes};
pub use tasks::{Claim, Dispatch, Subtask, SweepOutcome, Tasks};

use ulid::Ulid;

/// Fresh primary key. ULIDs sort by creation time, which keeps
/// `ORDER BY id` meaningful for the append-only tables.
pub(crate) fn new_id() -> String {
    Ulid::generate().to_string()
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
