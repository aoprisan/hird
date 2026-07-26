//! What the witness saw, and the contention detector built on it.
//!
//! Two tables. `task_witness` holds the working-tree fingerprint a task is
//! measured against, taken when it was claimed and never moved after that.
//! `task_changes` holds the difference between that fingerprint and the tree as
//! it stands — one row per path, rewritten on every observation, so a file
//! edited and then put back the way it was leaves no row behind.
//!
//! The `hash` on a change row is not simply "the content now". It is the last
//! version this task's own holder was *shown*, and that distinction is the
//! whole contention detector. Anybody may look — another agent's check-in, the
//! human's board — and looking updates the footprint without touching the
//! hashes; only [`Witnessed::confirm`], called on a holder's own check-in after
//! it has been handed the report, moves them. So when two rows for one path
//! disagree, the older one belongs to an agent whose copy of that file is out
//! of date, and a write from it would silently discard the other's work.
//!
//! What the footprint cannot supply on its own is *whose* change it was — a
//! shared checkout has one filesystem and no keyboards — so every live task
//! carries every change made while it was live. The declared scopes are the
//! other half: [`Witnessed::contention`] fires only where two live tasks both
//! said they would write a file and the file then moved, which is the predicted
//! collision and the observed one at the same time.

use rusqlite::{params, Connection, Row, Transaction, TransactionBehavior};

use super::deps::id_for_seq;
use super::{new_id, ProjectScope};
use crate::error::Result;
use crate::glob;
use crate::model::{now_ts, Contention, EventKind, Observed, Status, WitnessedTask};
use crate::witness::{Change, Tree};

/// A live task and the tree it is measured against.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub seq: i64,
    pub task_id: String,
    /// The tree as it stood when the task was claimed.
    pub tree: Tree,
}

/// Repository over `task_witness` and `task_changes`.
pub struct Witnessed<'a> {
    conn: &'a Connection,
}

impl<'a> Witnessed<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Witnessed<'a> {
        Witnessed { conn }
    }

    /// Start measuring task `seq` against `tree`.
    ///
    /// Called once, when the task is claimed. Re-claiming after a lease lapse
    /// starts a fresh measurement: the previous holder's footprint is not the
    /// new holder's doing, and keeping it would blame them for it.
    pub fn begin(&self, seq: i64, tree: &Tree) -> Result<()> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();
        let encoded = encode(tree);
        tx.execute("DELETE FROM task_changes WHERE task_id = ?1", [&task_id])?;
        tx.execute(
            "INSERT INTO task_witness (task_id, head, tree, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(task_id) DO UPDATE SET
               head = excluded.head, tree = excluded.tree, at = excluded.at",
            params![task_id, tree.head, encoded, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every task in `project` that currently holds a lease and has a baseline.
    ///
    /// This is what an observation needs before it takes place: the union of
    /// these trees' paths is what has to be looked at, and their commits are
    /// what a `HEAD` that has moved is compared against.
    pub fn baselines(&self, project: &str) -> Result<Vec<Baseline>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.seq, w.task_id, w.head, w.tree
             FROM task_witness w JOIN tasks t ON t.id = w.task_id
             WHERE t.project = ?1 AND t.status IN ('claimed','in_progress')
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map([project], |row| {
            Ok(Baseline {
                seq: row.get(0)?,
                task_id: row.get(1)?,
                tree: decode(row.get::<_, String>(2)?, row.get::<_, String>(3)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace what task `seq` is on record as having seen change.
    ///
    /// `changes` is the whole difference from the claim fingerprint, so a path
    /// that has dropped out of it has gone back to the way it started and its
    /// row goes with it. Paths appearing for the first time are returned, and
    /// only those, so the caller can log the new ones without writing an event
    /// on every heartbeat.
    ///
    /// Looking never counts as being told. A row already on file keeps the
    /// content it was last *confirmed* at, however many times anybody observes
    /// it in the meantime, and only [`Witnessed::confirm`] moves it — which is
    /// what lets [`Witnessed::contention`] tell a stale copy from a current
    /// one. A path seen here for the first time is recorded at what it says
    /// now, because there is no earlier version to preserve.
    pub fn record(&self, seq: i64, changes: &[Change], actor: &str) -> Result<Vec<String>> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();

        let mut known: Vec<String> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT path FROM task_changes WHERE task_id = ?1")?;
            let rows = stmt.query_map([&task_id], |row| row.get::<_, String>(0))?;
            for path in rows {
                known.push(path?);
            }
        }

        let mut fresh: Vec<String> = Vec::new();
        for change in changes {
            let kind = change.kind.as_str();
            if known.iter().any(|p| p == &change.path) {
                tx.execute(
                    "UPDATE task_changes SET kind = ?3 WHERE task_id = ?1 AND path = ?2",
                    params![task_id, change.path, kind],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO task_changes
                       (id, task_id, path, kind, hash, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![new_id(), task_id, change.path, kind, change.hash, now],
                )?;
                fresh.push(change.path.clone());
            }
        }

        for gone in known
            .iter()
            .filter(|path| !changes.iter().any(|c| &c.path == *path))
        {
            tx.execute(
                "DELETE FROM task_changes WHERE task_id = ?1 AND path = ?2",
                params![task_id, gone],
            )?;
        }

        if !fresh.is_empty() {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now,
                actor,
                EventKind::Witnessed,
                &format!("changed {}", fresh.join(", ")),
            )?;
        }
        tx.commit()?;
        Ok(fresh)
    }

    /// Record that task `seq`'s holder has now been shown these versions.
    ///
    /// Called after the holder has been handed the report and not before: an
    /// agent that is told "this file moved under you" in the same breath as
    /// having its copy marked current has been told nothing at all. So a
    /// check-in reads the evidence first, and confirms second.
    pub fn confirm(&self, seq: i64, changes: &[Change]) -> Result<()> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();
        for change in changes {
            tx.execute(
                "UPDATE task_changes SET hash = ?3, last_seen = ?4
                 WHERE task_id = ?1 AND path = ?2",
                params![task_id, change.path, change.hash, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// What the witness saw change while task `seq` was held.
    pub fn touched(&self, seq: i64) -> Result<Vec<Observed>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, hash, first_seen, last_seen FROM task_changes
             WHERE task_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([&task_id], row_to_observed)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Files two live tasks both said they would write, and that have since
    /// moved under both of them.
    ///
    /// This is the intersection of hird's two detectors, and it is deliberately
    /// narrower than either. A shared checkout cannot tell you who typed: when
    /// a file changes, every task that was live at the time has it in its
    /// footprint, so the footprint alone would accuse everybody of everything.
    /// A declaration is what supplies the missing half — an agent that said
    /// "I am going to write `src/config.rs`" has told you it holds a copy and
    /// intends to write from it, and when that file then moves, its copy is the
    /// one about to be written over the other agent's work.
    ///
    /// So: a predicted collision that the filesystem has confirmed. A task that
    /// declared nothing contends with nobody, which is one more reason to
    /// declare early and the only one that costs an agent its own work.
    ///
    /// Sweep leases before calling: a task whose lease has lapsed is not
    /// somebody else's problem any more.
    pub fn contention(&self, seq: i64) -> Result<Vec<Contention>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mine = super::scope::Scopes::new(self.conn).for_task(seq)?;
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        let project: String = self.conn.query_row(
            "SELECT project FROM tasks WHERE id = ?1",
            [&task_id],
            |row| row.get(0),
        )?;
        let live_scopes =
            super::scope::Scopes::new(self.conn).declared(&ProjectScope::Only(project), true)?;

        let mut stmt = self.conn.prepare(
            "SELECT mine.path, mine.hash, mine.last_seen,
                    t.seq, t.title, t.status, t.claimed_by,
                    theirs.hash, theirs.last_seen
             FROM task_changes mine
             JOIN task_changes theirs ON theirs.path = mine.path
                                     AND theirs.task_id <> mine.task_id
             JOIN tasks t ON t.id = theirs.task_id
             WHERE mine.task_id = ?1
               AND t.status IN ('claimed','in_progress')
               AND t.project = (SELECT project FROM tasks WHERE id = ?1)
             ORDER BY mine.path, t.seq",
        )?;
        let rows = stmt.query_map([&task_id], |row| {
            let raw: String = row.get(5)?;
            let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(Contention {
                path: row.get(0)?,
                hash: row.get(1)?,
                last_seen: row.get(2)?,
                other_seq: row.get(3)?,
                other_title: row.get(4)?,
                other_status: status,
                other_holder: row.get(6)?,
                other_hash: row.get(7)?,
                other_last_seen: row.get(8)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            let found: Contention = row?;
            let theirs = live_scopes
                .iter()
                .find(|s| s.seq == found.other_seq)
                .map(|s| s.patterns.as_slice())
                .unwrap_or_default();
            let staked =
                |patterns: &[String]| patterns.iter().any(|p| glob::matches(p, &found.path));
            // Both must have a stake, and the two must actually disagree about
            // what the file says. Two agents in one file who have both been
            // shown the same version are not in trouble yet, and saying so on
            // every heartbeat would drown out the time they are.
            if found.is_stale() && staked(&mine) && staked(theirs) {
                out.push(found);
            }
        }
        Ok(out)
    }

    /// Paths task `seq` was seen to change that none of its declared patterns
    /// describes.
    ///
    /// Drift, in other words: the gap between what an agent said it would touch
    /// and where it actually went. Worth surfacing rather than papering over,
    /// because every other agent's collision check is reading the declaration.
    pub fn undeclared(&self, seq: i64) -> Result<Vec<String>> {
        let patterns = super::scope::Scopes::new(self.conn).for_task(seq)?;
        if patterns.is_empty() {
            // Nothing was declared, so nothing has drifted from it. An agent
            // that never declared a scope is a different complaint.
            return Ok(Vec::new());
        }
        Ok(self
            .touched(seq)?
            .into_iter()
            .map(|o| o.path)
            .filter(|path| !patterns.iter().any(|p| glob::matches(p, path)))
            .collect())
    }

    /// Every task in `scope` the witness has something on, newest first.
    ///
    /// The radar's second data source: the TUI paints these next to what the
    /// same tasks declared, and the disagreements are the story.
    pub fn seen(&self, scope: &ProjectScope, active_only: bool) -> Result<Vec<WitnessedTask>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let active = if active_only {
            "AND t.status IN ('claimed','in_progress')"
        } else {
            ""
        };
        let sql = format!(
            "SELECT t.seq, t.title, t.status, t.claimed_by,
                    c.path, c.kind, c.hash, c.first_seen, c.last_seen
             FROM task_changes c JOIN tasks t ON t.id = c.task_id
             WHERE {project_clause} {active}
             ORDER BY t.seq, c.path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            let raw: String = row.get(2)?;
            let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let task = WitnessedTask {
                seq: row.get(0)?,
                title: row.get(1)?,
                status,
                holder: row.get(3)?,
                changes: Vec::new(),
            };
            let observed = Observed {
                path: row.get(4)?,
                kind: row.get(5)?,
                hash: row.get(6)?,
                first_seen: row.get(7)?,
                last_seen: row.get(8)?,
            };
            Ok((task, observed))
        })?;

        let mut out: Vec<WitnessedTask> = Vec::new();
        for row in rows {
            let (task, observed) = row?;
            match out.last_mut() {
                Some(last) if last.seq == task.seq => last.changes.push(observed),
                _ => out.push(WitnessedTask {
                    changes: vec![observed],
                    ..task
                }),
            }
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------- helpers

fn row_to_observed(row: &Row<'_>) -> rusqlite::Result<Observed> {
    Ok(Observed {
        path: row.get(0)?,
        kind: row.get(1)?,
        hash: row.get(2)?,
        first_seen: row.get(3)?,
        last_seen: row.get(4)?,
    })
}

/// Trees are stored as one JSON object of path to hash.
///
/// Opaque to SQL on purpose: nothing ever queries inside a fingerprint, it is
/// only ever loaded whole and subtracted from another one. `task_changes` is
/// where the queryable half lives.
fn encode(tree: &Tree) -> String {
    serde_json::to_string(&tree.entries).unwrap_or_else(|_| "{}".to_string())
}

fn decode(head: String, entries: String) -> Tree {
    Tree {
        head,
        entries: serde_json::from_str(&entries).unwrap_or_default(),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::witness::ChangeKind;
    use std::collections::BTreeMap;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, title: &str, holder: &str) -> i64 {
        let seq = db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq;
        db.tasks().claim(seq, holder, TTL).unwrap();
        seq
    }

    fn tree(entries: &[(&str, &str)]) -> Tree {
        Tree {
            head: "abc123".to_string(),
            entries: entries
                .iter()
                .map(|(p, h)| (p.to_string(), h.to_string()))
                .collect::<BTreeMap<_, _>>(),
            truncated: false,
        }
    }

    /// Say a task will touch `patterns`, the way an agent does on claiming.
    fn declare(db: &Db, seq: i64, patterns: &[&str]) {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        db.scopes()
            .declare(seq, &owned, "cli", super::super::OnConflict::Report)
            .unwrap();
    }

    fn change(path: &str, hash: &str) -> Change {
        Change {
            path: path.to_string(),
            kind: ChangeKind::Modified,
            hash: hash.to_string(),
        }
    }

    #[test]
    fn a_baseline_round_trips_through_the_database() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        let snapshot = tree(&[("src/a.rs", "hash-a"), ("src/b.rs", "")]);
        db.witnessed().begin(seq, &snapshot).unwrap();

        let live = db.witnessed().baselines(PROJECT).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].seq, seq);
        assert_eq!(live[0].tree, snapshot);
    }

    #[test]
    fn only_live_tasks_have_baselines_to_measure() {
        let db = db();
        let live = seed(&db, "live", "codex:9f2c");
        let finished = seed(&db, "finished", "codex:9f2c");
        db.witnessed().begin(live, &tree(&[])).unwrap();
        db.witnessed().begin(finished, &tree(&[])).unwrap();
        db.tasks().complete(finished, "codex:9f2c", "done").unwrap();

        let seqs: Vec<i64> = db
            .witnessed()
            .baselines(PROJECT)
            .unwrap()
            .into_iter()
            .map(|b| b.seq)
            .collect();
        assert_eq!(seqs, vec![live]);
    }

    #[test]
    fn recording_reports_only_paths_it_had_not_seen_before() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();

        let first = db
            .witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert_eq!(first, vec!["src/a.rs"]);

        // Same path, new content: not a new path, so no second event.
        let moved = [change("src/a.rs", "h2")];
        let again = db.witnessed().record(seq, &moved, "codex:9f2c").unwrap();
        assert!(again.is_empty());
        db.witnessed().confirm(seq, &moved).unwrap();

        let wider = db
            .witnessed()
            .record(
                seq,
                &[change("src/a.rs", "h2"), change("src/b.rs", "h3")],
                "codex:9f2c",
            )
            .unwrap();
        assert_eq!(wider, vec!["src/b.rs"]);

        let touched = db.witnessed().touched(seq).unwrap();
        assert_eq!(touched.len(), 2);
        assert_eq!(touched[0].path, "src/a.rs");
        assert_eq!(touched[0].hash, "h2");
        assert!(touched[0].first_seen <= touched[0].last_seen, "{touched:?}");
    }

    /// A file edited and then put back the way it was is not a change, and the
    /// record has to be able to say so — otherwise the contention detector
    /// keeps firing on work that was undone.
    #[test]
    fn a_path_that_goes_back_to_normal_leaves_no_row() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert_eq!(db.witnessed().touched(seq).unwrap().len(), 1);

        db.witnessed().record(seq, &[], "codex:9f2c").unwrap();
        assert!(db.witnessed().touched(seq).unwrap().is_empty());
    }

    #[test]
    fn the_first_sighting_of_a_path_is_written_into_the_history() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();

        let task = db.tasks().get(seq).unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        let witnessed: Vec<&crate::model::TaskEvent> = events
            .iter()
            .filter(|e| e.kind == EventKind::Witnessed)
            .collect();
        assert_eq!(witnessed.len(), 1);
        assert!(witnessed[0].detail.contains("src/a.rs"), "{witnessed:?}");
    }

    /// Looking is not telling. However many times the tree is observed, an
    /// agent's recorded version of a file only moves when that agent has
    /// actually been handed the new one.
    #[test]
    fn observing_does_not_confirm_a_version_but_confirming_does() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "mine")], "codex:9f2c")
            .unwrap();
        let first = db.witnessed().touched(seq).unwrap()[0].clone();

        let moved = [change("src/a.rs", "somebody-elses")];
        db.witnessed().record(seq, &moved, "tui").unwrap();
        let after = db.witnessed().touched(seq).unwrap()[0].clone();
        assert_eq!(after.hash, "mine", "looking cannot confirm a version");
        assert_eq!(after.last_seen, first.last_seen);

        db.witnessed().confirm(seq, &moved).unwrap();
        assert_eq!(
            db.witnessed().touched(seq).unwrap()[0].hash,
            "somebody-elses",
            "being shown the new version is what catches an agent up"
        );
    }

    #[test]
    fn two_live_tasks_in_one_file_are_a_contention() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed().begin(mine, &tree(&[])).unwrap();
        db.witnessed().begin(theirs, &tree(&[])).unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/*.rs"]);

        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "claude-code:af31")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "h2")], "codex:9f2c")
            .unwrap();

        let seen = db.witnessed().contention(mine).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "src/shared.rs");
        assert_eq!(seen[0].other_seq, theirs);
        assert_eq!(seen[0].other_holder.as_deref(), Some("codex:9f2c"));
        assert!(seen[0].is_stale(), "the hashes differ, so one side is old");

        let sentence = seen[0].describe();
        assert!(sentence.contains("src/shared.rs"), "{sentence}");
        assert!(sentence.contains("codex:9f2c"), "{sentence}");
        assert!(sentence.contains("re-read"), "{sentence}");
    }

    /// Two agents in one file who have both been shown the same version are
    /// not in trouble, and hearing about it on every heartbeat is how a
    /// warning stops being read.
    #[test]
    fn two_agents_who_agree_on_the_content_are_not_reported() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed().begin(mine, &tree(&[])).unwrap();
        db.witnessed().begin(theirs, &tree(&[])).unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "same")], "a")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "same")], "b")
            .unwrap();

        assert!(db.witnessed().contention(mine).unwrap().is_empty());

        // It becomes one the moment one of them is left behind.
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "moved on")], "b")
            .unwrap();
        db.witnessed()
            .confirm(theirs, &[change("src/shared.rs", "moved on")])
            .unwrap();
        let seen = db.witnessed().contention(mine).unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].is_stale());
    }

    #[test]
    fn a_finished_task_stops_contending() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed().begin(mine, &tree(&[])).unwrap();
        db.witnessed().begin(theirs, &tree(&[])).unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "a")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "h2")], "b")
            .unwrap();
        assert_eq!(db.witnessed().contention(mine).unwrap().len(), 1);

        db.tasks()
            .complete(theirs, "codex:9f2c", "shipped")
            .unwrap();
        assert!(db.witnessed().contention(mine).unwrap().is_empty());
    }

    #[test]
    fn tasks_in_other_projects_never_contend() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let elsewhere = db
            .tasks()
            .create("/other/project", "theirs", "", 0, "cli")
            .unwrap()
            .seq;
        db.tasks().claim(elsewhere, "codex:9f2c", TTL).unwrap();
        db.witnessed().begin(mine, &tree(&[])).unwrap();
        db.witnessed().begin(elsewhere, &tree(&[])).unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, elsewhere, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "a")
            .unwrap();
        db.witnessed()
            .record(elsewhere, &[change("src/shared.rs", "h2")], "b")
            .unwrap();

        assert!(db.witnessed().contention(mine).unwrap().is_empty());
    }

    #[test]
    fn drift_is_the_gap_between_what_was_declared_and_what_moved() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string()],
                "codex:9f2c",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[change("src/config.rs", "h1"), change("src/mcp.rs", "h2")],
                "codex:9f2c",
            )
            .unwrap();

        assert_eq!(db.witnessed().undeclared(seq).unwrap(), vec!["src/mcp.rs"]);
    }

    #[test]
    fn a_glob_covers_everything_it_describes() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/**".to_string()],
                "codex:9f2c",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[change("src/repo/tasks.rs", "h1"), change("README.md", "h2")],
                "codex:9f2c",
            )
            .unwrap();

        assert_eq!(db.witnessed().undeclared(seq).unwrap(), vec!["README.md"]);
    }

    #[test]
    fn declaring_nothing_cannot_drift() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.witnessed()
            .record(seq, &[change("src/anything.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert!(db.witnessed().undeclared(seq).unwrap().is_empty());
    }

    /// Re-claiming after a lease lapse must not hand the new holder the old
    /// holder's footprint — they did not do it, and they cannot answer for it.
    #[test]
    fn a_fresh_claim_starts_a_fresh_measurement() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[])).unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        db.tasks()
            .release(seq, "codex:9f2c", "handing back")
            .unwrap();

        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[("src/a.rs", "h1")]))
            .unwrap();
        assert!(db.witnessed().touched(seq).unwrap().is_empty());
    }

    #[test]
    fn the_radar_groups_changes_by_task() {
        let db = db();
        let a = seed(&db, "a", "codex:9f2c");
        let b = seed(&db, "b", "claude-code:af31");
        db.witnessed().begin(a, &tree(&[])).unwrap();
        db.witnessed().begin(b, &tree(&[])).unwrap();
        db.witnessed()
            .record(
                a,
                &[change("src/a.rs", "h1"), change("src/b.rs", "h2")],
                "a",
            )
            .unwrap();
        db.witnessed()
            .record(b, &[change("docs/index.html", "h3")], "b")
            .unwrap();
        db.tasks()
            .complete(b, "claude-code:af31", "shipped")
            .unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        let live = db.witnessed().seen(&scope, true).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].seq, a);
        assert_eq!(live[0].changes.len(), 2);

        let all = db.witnessed().seen(&scope, false).unwrap();
        assert_eq!(all.len(), 2);
    }
}
