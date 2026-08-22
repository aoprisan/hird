//! The recess: a per-project stand-down called by the human.
//!
//! Every hand-out path in hird is live all the time — `task_next` answers
//! whoever asks, and a dispatch hook summons a worker the moment a task
//! becomes claimable. The moment the human needs the tree to themselves (a
//! rebase, a merge landing, a plan they have stopped believing in) there was
//! no way to say so short of killing sessions or unwiring the hook. A recess
//! is that control: one row per project, and while it exists no claim is
//! handed out. Work already claimed is untouched — leases run, check-ins and
//! completions land — because a recess stops the hand-out, not the work.
//!
//! Calling and lifting a recess are human acts, like filing a plan, so there
//! is no MCP tool for either; agents meet the recess in claim refusals and in
//! `task_next` answers. The row is project-level state rather than a task
//! event, so it does not ride the task trail — the row itself is the record
//! while the recess stands, and the board wears it wherever it is shown.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::model::{now_ts, Recess};

/// What calling a recess did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Called {
    /// The queue stood down just now.
    Began(Recess),
    /// It was already standing; calling again is idempotent, though a new
    /// reason replaces the old one so the refusals stay current.
    AlreadyStanding {
        recess: Recess,
        reason_changed: bool,
    },
}

/// Repository over `project_recess`.
pub struct Recesses<'a> {
    conn: &'a Connection,
}

impl<'a> Recesses<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Recesses<'a> {
        Recesses { conn }
    }

    /// Stand `project`'s queue down, or refresh the reason it already stands
    /// under. `at` keeps its original value on a repeat call: the recess
    /// began when it began.
    pub fn call(&self, project: &str, reason: &str, actor: &str) -> Result<Called> {
        let reason = reason.trim();
        let tx = super::immediate_tx(self.conn)?;
        let outcome = match current_in(&tx, project)? {
            Some(standing) => {
                let reason_changed = standing.reason != reason;
                if reason_changed {
                    tx.execute(
                        "UPDATE project_recess SET reason = ?1 WHERE project = ?2",
                        params![reason, project],
                    )?;
                }
                Called::AlreadyStanding {
                    recess: Recess {
                        reason: reason.to_string(),
                        ..standing
                    },
                    reason_changed,
                }
            }
            None => {
                let recess = Recess {
                    project: project.to_string(),
                    reason: reason.to_string(),
                    actor: actor.to_string(),
                    at: now_ts(),
                };
                tx.execute(
                    "INSERT INTO project_recess (project, reason, actor, at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![recess.project, recess.reason, recess.actor, recess.at],
                )?;
                Called::Began(recess)
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Lift `project`'s recess. Returns what stood, or `None` when the queue
    /// was not in recess — which is not an error, only nothing to lift.
    pub fn lift(&self, project: &str) -> Result<Option<Recess>> {
        let tx = super::immediate_tx(self.conn)?;
        let standing = current_in(&tx, project)?;
        if standing.is_some() {
            tx.execute(
                "DELETE FROM project_recess WHERE project = ?1",
                params![project],
            )?;
        }
        tx.commit()?;
        Ok(standing)
    }

    /// The recess `project` currently stands under, if any.
    pub fn current(&self, project: &str) -> Result<Option<Recess>> {
        current_in(self.conn, project)
    }
}

/// The recess `project` stands under, read inside a caller-owned transaction
/// or plain connection. This is what the claim gates call.
pub(crate) fn current_in(conn: &Connection, project: &str) -> Result<Option<Recess>> {
    Ok(conn
        .query_row(
            "SELECT project, reason, actor, at FROM project_recess WHERE project = ?1",
            [project],
            |row| {
                Ok(Recess {
                    project: row.get(0)?,
                    reason: row.get(1)?,
                    actor: row.get(2)?,
                    at: row.get(3)?,
                })
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const PROJECT: &str = "/tmp/project";

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn calling_a_recess_stands_the_project_down() {
        let db = db();
        assert!(db.recesses().current(PROJECT).unwrap().is_none());

        let called = db.recesses().call(PROJECT, "rebasing main", "cli").unwrap();
        let Called::Began(recess) = called else {
            panic!("first call must begin the recess");
        };
        assert_eq!(recess.project, PROJECT);
        assert_eq!(recess.reason, "rebasing main");
        assert_eq!(recess.actor, "cli");

        let standing = db.recesses().current(PROJECT).unwrap().unwrap();
        assert_eq!(standing, recess);
        assert!(
            db.recesses().current("/tmp/elsewhere").unwrap().is_none(),
            "a recess is per project"
        );
    }

    #[test]
    fn calling_again_is_idempotent_but_refreshes_the_reason() {
        let db = db();
        db.recesses().call(PROJECT, "rebasing main", "cli").unwrap();
        let began_at = db.recesses().current(PROJECT).unwrap().unwrap().at;

        let repeat = db.recesses().call(PROJECT, "rebasing main", "cli").unwrap();
        assert!(matches!(
            repeat,
            Called::AlreadyStanding {
                reason_changed: false,
                ..
            }
        ));

        let reworded = db
            .recesses()
            .call(PROJECT, "merging the PR", "cli")
            .unwrap();
        let Called::AlreadyStanding {
            recess,
            reason_changed: true,
        } = reworded
        else {
            panic!("a new reason must be reported as a change");
        };
        assert_eq!(recess.reason, "merging the PR");
        assert_eq!(recess.at, began_at, "rewording does not restart the recess");
        assert_eq!(
            db.recesses().current(PROJECT).unwrap().unwrap().reason,
            "merging the PR"
        );
    }

    #[test]
    fn lifting_returns_what_stood_and_only_once() {
        let db = db();
        db.recesses().call(PROJECT, "rebasing", "cli").unwrap();

        let lifted = db.recesses().lift(PROJECT).unwrap().unwrap();
        assert_eq!(lifted.reason, "rebasing");
        assert!(db.recesses().current(PROJECT).unwrap().is_none());
        assert!(
            db.recesses().lift(PROJECT).unwrap().is_none(),
            "lifting nothing is nothing, not an error"
        );
    }

    #[test]
    fn the_refusal_sentence_carries_the_reason_when_there_is_one() {
        let with = Recess {
            project: PROJECT.into(),
            reason: "rebasing main".into(),
            actor: "cli".into(),
            at: now_ts(),
        };
        assert_eq!(
            with.describe(),
            "the human stood this queue down (\"rebasing main\") — nothing is handed out \
             until `hird resume` lifts it, though work already claimed continues"
        );
        let without = Recess {
            reason: String::new(),
            ..with
        };
        assert!(without
            .describe()
            .starts_with("the human stood this queue down — nothing"));
    }
}
